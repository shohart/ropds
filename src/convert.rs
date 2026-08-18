use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::ConvertConfig;

/// Monotonic counter used to make temp filenames unique within a process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    #[error("conversion is disabled")]
    Disabled,
    #[error("unsupported target format: {0}")]
    UnsupportedFormat(String),
    #[error("no converter command configured")]
    NoConverter,
    #[error("failed to create temp dir: {0}")]
    TempDir(std::io::Error),
    #[error("failed to write input file: {0}")]
    WriteInput(std::io::Error),
    #[error("failed to spawn converter: {0}")]
    Spawn(std::io::Error),
    #[error("converter exited with status: {0}")]
    ConverterFailed(String),
    #[error("conversion timed out after {0}s")]
    Timeout(u64),
    #[error("converter produced no output file")]
    NoOutput,
    #[error("failed to read output file: {0}")]
    ReadOutput(std::io::Error),
}

/// Convert an FB2 book (bytes) to a target format by shelling out to a
/// configured external converter command.
///
/// `target_format` must be present in `config.formats` and consist only of
/// ASCII alphanumeric characters (it becomes part of the output filename).
pub async fn convert(
    config: &ConvertConfig,
    input_bytes: &[u8],
    input_filename: &str,
    target_format: &str,
) -> Result<Vec<u8>, ConvertError> {
    if !config.enabled {
        return Err(ConvertError::Disabled);
    }
    if !config.formats.iter().any(|f| f == target_format) {
        return Err(ConvertError::UnsupportedFormat(target_format.to_string()));
    }
    if !target_format.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ConvertError::UnsupportedFormat(target_format.to_string()));
    }
    if config.command.trim().is_empty() {
        return Err(ConvertError::NoConverter);
    }

    // Unique, sanitized base name to avoid collisions between concurrent
    // conversions and across processes sharing the same temp dir.
    let base = format!(
        "{}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        filename_stem(input_filename)
    );
    let in_name = format!("{base}.fb2");
    let out_name = format!("{base}.{target_format}");

    std::fs::create_dir_all(&config.temp_dir).map_err(ConvertError::TempDir)?;
    let in_path = config.temp_dir.join(&in_name);
    let out_path = config.temp_dir.join(&out_name);

    std::fs::write(&in_path, input_bytes).map_err(ConvertError::WriteInput)?;

    let command = config
        .command
        .replace("{input}", &shell_quote(&in_path))
        .replace("{output}", &shell_quote(&out_path));

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(config.timeout_secs),
        run_command(&command),
    )
    .await;

    let status = match result {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let _ = std::fs::remove_file(&in_path);
            let _ = std::fs::remove_file(&out_path);
            return Err(ConvertError::Spawn(e));
        }
        Err(_elapsed) => {
            let _ = std::fs::remove_file(&in_path);
            let _ = std::fs::remove_file(&out_path);
            return Err(ConvertError::Timeout(config.timeout_secs));
        }
    };

    if !status.success() {
        let _ = std::fs::remove_file(&in_path);
        let _ = std::fs::remove_file(&out_path);
        return Err(ConvertError::ConverterFailed(format!(
            "exit status: {status}"
        )));
    }

    let output = match std::fs::read(&out_path) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => {
            let _ = std::fs::remove_file(&in_path);
            let _ = std::fs::remove_file(&out_path);
            return Err(ConvertError::NoOutput);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&in_path);
            let _ = std::fs::remove_file(&out_path);
            return Err(ConvertError::ReadOutput(e));
        }
    };

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);

    Ok(output)
}

/// Run a converter command via `sh -c`, discarding its stdout/stderr.
async fn run_command(command: &str) -> std::io::Result<std::process::ExitStatus> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child.wait().await
}

/// Derive a safe base name (letters, digits, `-`, `_`) from a filename.
fn filename_stem(filename: &str) -> String {
    let base = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("book");
    let safe: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "book".to_string()
    } else {
        safe
    }
}

/// Quote a path for use inside a `sh -c` command string.
fn shell_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> ConvertConfig {
        let mut cfg = ConvertConfig::default();
        cfg.enabled = true;
        cfg.temp_dir = std::env::temp_dir().join("ropds-convert-test");
        cfg
    }

    #[test]
    fn test_filename_stem_sanitizes_and_preserves_unicode() {
        assert_eq!(filename_stem("Война и мир.fb2"), "Война_и_мир");
        // On Linux `\` is a literal filename char (not a separator), sanitized to `_`.
        assert_eq!(filename_stem("a/b\\c.fb2"), "b_c");
        assert_eq!(filename_stem("***.fb2"), "___");
        // Empty path (no stem) falls back to "book".
        assert_eq!(filename_stem(""), "book");
    }

    #[test]
    fn test_shell_quote() {
        let p = Path::new("/tmp/ropds-convert/1_0_book.fb2");
        assert_eq!(shell_quote(p), "\"/tmp/ropds-convert/1_0_book.fb2\"");
    }

    #[tokio::test]
    async fn test_convert_disabled() {
        let cfg = ConvertConfig::default(); // enabled = false
        let err = convert(&cfg, b"<FictionBook/>", "book.fb2", "epub")
            .await
            .unwrap_err();
        assert!(matches!(err, ConvertError::Disabled));
    }

    #[tokio::test]
    async fn test_convert_unsupported_format() {
        let cfg = test_config();
        let err = convert(&cfg, b"<FictionBook/>", "book.fb2", "docx")
            .await
            .unwrap_err();
        assert!(matches!(err, ConvertError::UnsupportedFormat(_)));
    }

    #[tokio::test]
    async fn test_convert_rejects_unsafe_format() {
        let mut cfg = test_config();
        cfg.formats = vec!["epub".to_string(), "bad/../format".to_string()];
        let err = convert(&cfg, b"x", "book.fb2", "bad/../format")
            .await
            .unwrap_err();
        assert!(matches!(err, ConvertError::UnsupportedFormat(_)));
    }

    #[tokio::test]
    async fn test_convert_no_converter_command() {
        let mut cfg = test_config();
        cfg.command = String::new();
        let err = convert(&cfg, b"x", "book.fb2", "epub").await.unwrap_err();
        assert!(matches!(err, ConvertError::NoConverter));
    }

    #[tokio::test]
    async fn test_convert_roundtrip_with_cp_converter() {
        let mut cfg = test_config();
        cfg.command = "cp \"{input}\" \"{output}\"".to_string();

        let out = convert(&cfg, b"hello-fb2", "book.fb2", "epub")
            .await
            .unwrap();
        assert_eq!(out, b"hello-fb2");
    }

    #[tokio::test]
    async fn test_convert_no_output_when_converter_fails() {
        let mut cfg = test_config();
        cfg.command = "false".to_string();

        let err = convert(&cfg, b"x", "book.fb2", "epub").await.unwrap_err();
        assert!(matches!(err, ConvertError::ConverterFailed(_)));
    }
}
