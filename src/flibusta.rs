use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fs2::FileExt;
use futures_util::StreamExt;
use regex::Regex;
use serde::Serialize;

use crate::config::{Config, FlibustaConfig};

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("updater is disabled")]
    Disabled,
    #[error("another updater is already running")]
    AlreadyRunning,
    #[error("invalid updater configuration: {0}")]
    Config(String),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ZIP validation failed: {0}")]
    Zip(String),
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateResult {
    pub discovered: usize,
    pub existing: usize,
    pub downloaded: usize,
    pub downloaded_bytes: u64,
    pub files: Vec<String>,
    pub dry_run: bool,
}

#[derive(Serialize)]
struct UpdateState<'a> {
    version: u8,
    updated_at: String,
    status: &'a str,
    result: &'a UpdateResult,
    error: Option<String>,
}

pub fn build_http_client(
    proxy_url: &str,
    timeout_secs: u64,
) -> Result<reqwest::Client, UpdateError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent("ROPDS-Flibusta-Updater/1.0");
    if !proxy_url.trim().is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url.trim())?);
    }
    Ok(builder.build()?)
}

pub async fn run(config: &Config, dry_run: bool, force: bool) -> Result<UpdateResult, UpdateError> {
    if !config.flibusta.enabled && !force {
        return Err(UpdateError::Disabled);
    }
    let destination = destination(config);
    ensure_inside_root(&config.library.root_path, &destination)?;
    fs::create_dir_all(&destination)?;
    let control = destination.join(".ropds-control");
    fs::create_dir_all(&control)?;
    let lock = File::create(control.join("flibusta-update.lock"))?;
    lock.try_lock_exclusive()
        .map_err(|_| UpdateError::AlreadyRunning)?;

    let mut result = UpdateResult {
        dry_run,
        ..Default::default()
    };
    let outcome = run_locked(config, &destination, &mut result).await;
    let (status, error) = match &outcome {
        Ok(_) => (if dry_run { "dry-run" } else { "success" }, None),
        Err(e) => ("failed", Some(e.to_string())),
    };
    write_state(&control, status, &result, error)?;
    let _ = lock.unlock();
    outcome.map(|_| result)
}

async fn run_locked(
    config: &Config,
    destination: &Path,
    result: &mut UpdateResult,
) -> Result<(), UpdateError> {
    let client = build_http_client(&config.flibusta.proxy_url, config.flibusta.timeout_secs)?;
    let urls = discover_with_retry(&client, &config.flibusta).await?;
    result.discovered = urls.len();
    let mut pending = Vec::new();
    for (name, url) in urls {
        if destination.join(&name).is_file() {
            result.existing += 1;
        } else {
            pending.push((name, url));
        }
    }
    result.files = pending.iter().map(|(name, _)| name.clone()).collect();
    if result.dry_run || pending.is_empty() {
        return Ok(());
    }

    let staging_root = destination.join(".ropds-update-staging");
    fs::create_dir_all(&staging_root)?;
    let staging = staging_root.join(format!(
        "run-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp()
    ));
    fs::create_dir_all(&staging)?;
    let mut staged = Vec::new();
    let operation = async {
        for (name, url) in pending {
            ensure_free_space(&staging, config.flibusta.min_free_space_mb)?;
            let path = staging.join(&name);
            let size = download_with_retry(&client, &config.flibusta, &url, &path).await?;
            if config.flibusta.validate_zip {
                let validate_path = path.clone();
                tokio::task::spawn_blocking(move || validate_zip(&validate_path))
                    .await
                    .map_err(|e| UpdateError::Zip(e.to_string()))??;
            }
            staged.push((name, path, size));
            let total: u64 = staged.iter().map(|(_, _, size)| *size).sum();
            if total > config.flibusta.max_total_size_mb * 1024 * 1024 {
                return Err(UpdateError::Config(
                    "update exceeds max_total_size_mb".into(),
                ));
            }
        }
        for (name, path, size) in &staged {
            let target = destination.join(name);
            if target.exists() {
                return Err(UpdateError::Config(format!(
                    "destination appeared: {}",
                    target.display()
                )));
            }
            fs::rename(path, &target)?;
            result.downloaded += 1;
            result.downloaded_bytes += size;
        }
        Ok(())
    }
    .await;
    let _ = fs::remove_dir_all(&staging);
    operation
}

async fn discover_with_retry(
    client: &reqwest::Client,
    cfg: &FlibustaConfig,
) -> Result<BTreeMap<String, reqwest::Url>, UpdateError> {
    let mut last = String::new();
    for attempt in 0..cfg.retries {
        match discover(client, cfg).await {
            Ok(urls) if !urls.is_empty() => return Ok(urls),
            Ok(_) => last = "index contained no matching archive links".into(),
            Err(e) => last = e.to_string(),
        }
        if attempt + 1 < cfg.retries {
            tokio::time::sleep(Duration::from_secs(1u64 << attempt.min(3))).await;
        }
    }
    Err(UpdateError::Config(format!("failed to read index: {last}")))
}

async fn discover(
    client: &reqwest::Client,
    cfg: &FlibustaConfig,
) -> Result<BTreeMap<String, reqwest::Url>, UpdateError> {
    let response = client
        .get(&cfg.source_url)
        .send()
        .await?
        .error_for_status()?;
    let base = response.url().clone();
    let source_host = base.host_str().map(str::to_string);
    let html = response.text().await?;
    let href_re = Regex::new(r#"(?i)href\s*=\s*["']([^"']+)["']"#)
        .map_err(|e| UpdateError::Config(e.to_string()))?;
    let file_re = Regex::new(&cfg.file_pattern).map_err(|e| UpdateError::Config(e.to_string()))?;
    let safe_re = Regex::new(r"^[A-Za-z0-9._-]+$").unwrap();
    let mut result = BTreeMap::new();
    for cap in href_re.captures_iter(&html) {
        let Ok(url) = base.join(&cap[1]) else {
            continue;
        };
        if url.host_str().map(str::to_string) != source_host {
            continue;
        }
        let Some(name) = url.path_segments().and_then(|mut s| s.next_back()) else {
            continue;
        };
        if safe_re.is_match(name) && file_re.is_match(name) {
            result.insert(name.to_string(), url);
        }
    }
    Ok(result)
}

async fn download_with_retry(
    client: &reqwest::Client,
    cfg: &FlibustaConfig,
    url: &reqwest::Url,
    path: &Path,
) -> Result<u64, UpdateError> {
    let mut last = String::new();
    for attempt in 0..cfg.retries {
        let _ = fs::remove_file(path);
        match download(client, cfg, url, path).await {
            Ok(size) => return Ok(size),
            Err(e) => last = e.to_string(),
        }
        if attempt + 1 < cfg.retries {
            tokio::time::sleep(Duration::from_secs(1u64 << attempt.min(3))).await;
        }
    }
    Err(UpdateError::Config(format!("download failed: {last}")))
}

async fn download(
    client: &reqwest::Client,
    cfg: &FlibustaConfig,
    url: &reqwest::Url,
    path: &Path,
) -> Result<u64, UpdateError> {
    let response = client.get(url.clone()).send().await?.error_for_status()?;
    let limit = cfg.max_file_size_mb * 1024 * 1024;
    if response.content_length().is_some_and(|v| v > limit) {
        return Err(UpdateError::Config(
            "archive exceeds max_file_size_mb".into(),
        ));
    }
    let mut file = File::create(path)?;
    let mut total = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        if total > limit {
            return Err(UpdateError::Config(
                "archive exceeds max_file_size_mb".into(),
            ));
        }
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    if total == 0 {
        return Err(UpdateError::Config("empty archive response".into()));
    }
    Ok(total)
}

fn validate_zip(path: &Path) -> Result<(), UpdateError> {
    let file = File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| UpdateError::Zip(e.to_string()))?;
    if archive.is_empty() {
        return Err(UpdateError::Zip("empty ZIP".into()));
    }
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| UpdateError::Zip(e.to_string()))?;
        std::io::copy(&mut entry, &mut std::io::sink()).map_err(UpdateError::Io)?;
    }
    Ok(())
}

fn ensure_free_space(path: &Path, min_mb: u64) -> Result<(), UpdateError> {
    let free = fs2::available_space(path)?;
    if free < min_mb * 1024 * 1024 {
        return Err(UpdateError::Config(format!(
            "insufficient free space: {} MiB",
            free / 1024 / 1024
        )));
    }
    Ok(())
}

fn destination(config: &Config) -> PathBuf {
    if config.flibusta.destination.as_os_str().is_empty() {
        config.library.root_path.clone()
    } else {
        config.flibusta.destination.clone()
    }
}

fn ensure_inside_root(root: &Path, destination: &Path) -> Result<(), UpdateError> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let destination = fs::canonicalize(destination).unwrap_or_else(|_| destination.to_path_buf());
    if !destination.starts_with(&root) {
        return Err(UpdateError::Config(
            "destination must be inside library root".into(),
        ));
    }
    Ok(())
}

fn write_state(
    control: &Path,
    status: &str,
    result: &UpdateResult,
    error: Option<String>,
) -> Result<(), UpdateError> {
    let state = UpdateState {
        version: 1,
        updated_at: chrono::Utc::now().to_rfc3339(),
        status,
        result,
        error,
    };
    let path = control.join("flibusta-update-state.json");
    let temp = control.join("flibusta-update-state.json.tmp");
    fs::write(
        &temp,
        serde_json::to_vec_pretty(&state).map_err(|e| UpdateError::Config(e.to_string()))?,
    )?;
    fs::rename(temp, path)?;
    Ok(())
}

pub fn should_run_now(cfg: &FlibustaConfig, now: chrono::DateTime<chrono::Local>) -> bool {
    use chrono::{Datelike, Timelike};
    (cfg.schedule_minutes.is_empty() || cfg.schedule_minutes.contains(&now.minute()))
        && (cfg.schedule_hours.is_empty() || cfg.schedule_hours.contains(&now.hour()))
        && (cfg.schedule_day_of_week.is_empty()
            || cfg
                .schedule_day_of_week
                .contains(&now.weekday().number_from_monday()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn schedule_matches_expected_time() {
        let cfg = FlibustaConfig::default();
        let at_two = chrono::Local
            .with_ymd_and_hms(2026, 8, 18, 2, 0, 0)
            .unwrap();
        let at_three = chrono::Local
            .with_ymd_and_hms(2026, 8, 18, 3, 0, 0)
            .unwrap();
        assert!(should_run_now(&cfg, at_two));
        assert!(!should_run_now(&cfg, at_three));
    }

    #[test]
    fn default_pattern_matches_daily_fb2_only() {
        let cfg = FlibustaConfig::default();
        let re = Regex::new(&cfg.file_pattern).unwrap();
        assert!(re.is_match("f.fb2.885852-885911.zip"));
        assert!(!re.is_match("f.n.885852-885911.zip"));
        assert!(!re.is_match("../evil.zip"));
    }

    #[test]
    fn clients_accept_direct_http_and_socks_proxy() {
        assert!(build_http_client("", 30).is_ok());
        assert!(build_http_client("http://127.0.0.1:3128", 30).is_ok());
        assert!(build_http_client("socks5h://127.0.0.1:1080", 30).is_ok());
        assert!(crate::telegram::build_client("socks5://127.0.0.1:1080").is_ok());
    }
}
