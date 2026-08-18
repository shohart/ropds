use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use regex::Regex;
use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::DbPool;
use crate::db::queries::{authors, books};

/// In-memory pagination state for search results, keyed by chat.
#[derive(Clone)]
struct SearchState {
    query: String,
    page: usize,
}

type SearchCache = Arc<Mutex<HashMap<ChatId, SearchState>>>;

pub fn build_client(proxy_url: &str) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(60));
    if !proxy_url.trim().is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url.trim())?);
    }
    builder.build()
}

pub async fn run(pool: DbPool, config: Config) {
    if !config.telegram.enabled {
        return;
    }
    let client = match build_client(&config.telegram.proxy_url) {
        Ok(client) => client,
        Err(e) => {
            warn!("Telegram client configuration failed: {e}");
            return;
        }
    };
    let bot = Bot::with_client(config.telegram.token.clone(), client);
    let config = Arc::new(config);
    let search_cache: SearchCache = Arc::new(Mutex::new(HashMap::new()));
    info!("Telegram bot started");

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![pool, config, search_cache])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn handle_message(
    bot: Bot,
    msg: Message,
    pool: DbPool,
    config: Arc<Config>,
    search_cache: SearchCache,
) -> ResponseResult<()> {
    if !allowed_message(&msg, &config) {
        bot.send_message(msg.chat.id, "⛔ Доступ запрещён").await?;
        return Ok(());
    }
    let Some(text) = msg.text().map(str::trim) else {
        return Ok(());
    };
    if text == "/start" || text == "/help" {
        bot.send_message(
            msg.chat.id,
            "📚 <b>ROPDS — домашняя библиотека</b>\n\n\
             🔎 Отправьте часть названия книги или имя автора.\n\
             👆 Нажмите на книгу в списке и выберите формат.",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }
    if text.chars().count() < 3 {
        bot.send_message(msg.chat.id, "⚠️ Для поиска нужно минимум 3 символа.")
            .await?;
        return Ok(());
    }

    let page_size = config.telegram.max_results.max(1) as usize;
    let total = books::count_title_or_author(&pool, text).await.unwrap_or(0);
    if total == 0 {
        bot.send_message(
            msg.chat.id,
            "❌ Ничего не найдено. Попробуйте другой запрос.",
        )
        .await?;
        return Ok(());
    }

    let page = 0usize;
    let total_pages = page_count(total, page_size as i64);
    let found = books::search_title_or_author(
        &pool,
        text,
        page_size as i64,
        page as i64 * page_size as i64,
    )
    .await
    .unwrap_or_default();

    if let Ok(mut cache) = search_cache.lock() {
        cache.insert(
            msg.chat.id,
            SearchState {
                query: text.to_string(),
                page,
            },
        );
    }

    let (body, markup) =
        build_search_message(&pool, text, page, page_size, total, total_pages, &found).await?;
    bot.send_message(msg.chat.id, body)
        .parse_mode(ParseMode::Html)
        .reply_markup(markup)
        .await?;
    Ok(())
}

async fn send_book_card(
    bot: &Bot,
    chat_id: ChatId,
    pool: &DbPool,
    config: &Config,
    id: i64,
) -> ResponseResult<()> {
    let Ok(Some(book)) = books::get_by_id(pool, id).await else {
        bot.send_message(chat_id, "❌ Книга не найдена.").await?;
        return Ok(());
    };
    let names = authors::get_for_book(pool, id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.full_name)
        .collect::<Vec<_>>()
        .join(", ");

    // Format buttons in a two-column grid — format name + characteristic emoji.
    let mut buttons: Vec<InlineKeyboardButton> = Vec::new();
    buttons.push(InlineKeyboardButton::callback(
        format!(
            "{} {}",
            format_emoji(&book.format),
            book.format.to_uppercase()
        ),
        format!("get:{id}:orig"),
    ));
    if book.format == "fb2" && config.convert.enabled {
        for format in &config.convert.formats {
            buttons.push(InlineKeyboardButton::callback(
                format!("{} {}", format_emoji(format), format.to_uppercase()),
                format!("get:{id}:{format}"),
            ));
        }
    }
    let rows: Vec<Vec<InlineKeyboardButton>> =
        buttons.chunks(2).map(|chunk| chunk.to_vec()).collect();

    let annotation = strip_html(&book.annotation);
    let text = format!(
        "📖 <b>{}</b>\n✍️ {}\n\n<pre>Год:    {}\nЯзык:   {}\nРазмер: {} КБ\nФормат: {}</pre>{}",
        escape_html(&book.title),
        escape_html(if names.is_empty() { "—" } else { &names }),
        escape_html(if book.docdate.is_empty() {
            "—"
        } else {
            &book.docdate
        }),
        escape_html(if book.lang.is_empty() {
            "—"
        } else {
            &book.lang
        }),
        book.size / 1024,
        escape_html(&book.format.to_uppercase()),
        if annotation.is_empty() {
            String::new()
        } else {
            format!(
                "\n📝 <b>Аннотация:</b>\n<tg-spoiler>{}</tg-spoiler>",
                escape_html(&annotation.chars().take(2500).collect::<String>())
            )
        }
    );
    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(InlineKeyboardMarkup::new(rows))
        .await?;
    Ok(())
}

async fn build_search_message(
    pool: &DbPool,
    query: &str,
    page: usize,
    page_size: usize,
    total: i64,
    total_pages: usize,
    book_list: &[crate::db::models::Book],
) -> ResponseResult<(String, InlineKeyboardMarkup)> {
    let start_index = page * page_size;

    let mut lines: Vec<String> = Vec::new();
    let mut number_buttons: Vec<InlineKeyboardButton> = Vec::new();

    // Per-page caches so the same author keeps one emoji, and different authors cycle.
    let mut author_emoji_cache: HashMap<String, String> = HashMap::new();
    let mut male_idx = 0usize;
    let mut female_idx = 0usize;

    for (i, book) in book_list.iter().enumerate() {
        let number = start_index + i + 1;
        let book_emoji = BOOK_EMOJIS[i % BOOK_EMOJIS.len()];

        let book_authors = authors::get_for_book(pool, book.id)
            .await
            .unwrap_or_default();
        let author_names: Vec<String> = book_authors.iter().map(|a| a.full_name.clone()).collect();
        let author_display = if author_names.is_empty() {
            "—".to_string()
        } else {
            author_names.join(", ")
        };
        let primary = author_names.first().map(String::as_str).unwrap_or("");

        let author_emoji = if let Some(e) = author_emoji_cache.get(primary) {
            e.clone()
        } else {
            let (palette, counter) = match author_gender(primary) {
                Gender::Female => (FEMALE_EMOJIS.as_slice(), &mut female_idx),
                Gender::Male => (MALE_EMOJIS.as_slice(), &mut male_idx),
            };
            let e = palette[*counter % palette.len()].to_string();
            *counter += 1;
            author_emoji_cache.insert(primary.to_string(), e.clone());
            e
        };

        let year = extract_year(&book.docdate);
        let indent = author_line_indent(number);
        let mut second_line = format!("{indent}{author_emoji} {}", escape_html(&author_display));
        if !year.is_empty() {
            second_line.push_str(&format!(", {year}"));
        }

        lines.push(format!(
            "<b>{number}.</b> {book_emoji} <b>{}</b>\n{}",
            escape_html(&book.title),
            second_line,
        ));
        number_buttons.push(InlineKeyboardButton::callback(
            bold_digits(number),
            format!("card:{}", book.id),
        ));
    }

    let mut rows: Vec<Vec<InlineKeyboardButton>> = number_buttons
        .chunks(5)
        .map(|chunk| chunk.to_vec())
        .collect();

    if total_pages > 1 {
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(InlineKeyboardButton::callback("⬅️ Назад", "pg:prev"));
        }
        if page + 1 < total_pages {
            nav.push(InlineKeyboardButton::callback("Вперёд ➡️", "pg:next"));
        }
        if !nav.is_empty() {
            rows.push(nav);
        }
    }

    let body = format!(
        "🔎 <b>Результаты по запросу «{}»</b>\n📚 Найдено: {} · Страница {} из {}\n\n{}",
        escape_html(query),
        total,
        page + 1,
        total_pages.max(1),
        lines.join("\n\n"),
    );

    Ok((body, InlineKeyboardMarkup::new(rows)))
}

async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    pool: DbPool,
    config: Arc<Config>,
    search_cache: SearchCache,
) -> ResponseResult<()> {
    if !allowed_username(query.from.username.as_deref(), &config) {
        bot.answer_callback_query(query.id)
            .text("Доступ запрещён")
            .await?;
        return Ok(());
    }
    let Some(data) = query.data.as_deref() else {
        return Ok(());
    };
    let Some(message) = query.message else {
        return Ok(());
    };
    let chat_id = message.chat().id;

    // Candidate tap → open the book card.
    if let Some(id) = data.strip_prefix("card:") {
        bot.answer_callback_query(query.id).await?;
        if let Ok(id) = id.parse::<i64>() {
            send_book_card(&bot, chat_id, &pool, &config, id).await?;
        }
        return Ok(());
    }

    // Pagination.
    if data == "pg:prev" || data == "pg:next" {
        bot.answer_callback_query(query.id).await?;
        let page_size = config.telegram.max_results.max(1) as usize;

        // Read and update state under a short lock; never hold it across await.
        let query_text = {
            let mut cache = search_cache.lock().unwrap();
            match cache.get_mut(&chat_id) {
                Some(state) => {
                    state.page = if data == "pg:next" {
                        state.page.saturating_add(1)
                    } else {
                        state.page.saturating_sub(1)
                    };
                    Some(state.query.clone())
                }
                None => None,
            }
        };
        let Some(query_text) = query_text else {
            bot.send_message(
                chat_id,
                "⚠️ Сессия поиска устарела — введите запрос заново.",
            )
            .await?;
            return Ok(());
        };

        let total = books::count_title_or_author(&pool, &query_text)
            .await
            .unwrap_or(0);
        let total_pages = page_count(total, page_size as i64).max(1);
        let page = {
            let mut cache = search_cache.lock().unwrap();
            let page = match cache.get_mut(&chat_id) {
                Some(state) => {
                    state.page = state.page.min(total_pages - 1);
                    state.page
                }
                None => 0,
            };
            page
        };

        let found = books::search_title_or_author(
            &pool,
            &query_text,
            page_size as i64,
            page as i64 * page_size as i64,
        )
        .await
        .unwrap_or_default();

        let (body, markup) = build_search_message(
            &pool,
            &query_text,
            page,
            page_size,
            total,
            total_pages,
            &found,
        )
        .await?;

        bot.edit_message_text(chat_id, message.id(), body)
            .parse_mode(ParseMode::Html)
            .reply_markup(markup)
            .await?;
        return Ok(());
    }

    // Download request `get:{id}:{format}`.
    let parts: Vec<&str> = data.split(':').collect();
    if parts.len() != 3 || parts[0] != "get" {
        return Ok(());
    }
    let Ok(id) = parts[1].parse::<i64>() else {
        return Ok(());
    };
    let Ok(Some(book)) = books::get_by_id(&pool, id).await else {
        return Ok(());
    };
    bot.answer_callback_query(query.id)
        .text("Готовлю файл…")
        .await?;
    let root = &config.library.root_path;
    let Ok(original) = crate::opds::download::read_book_file(
        root,
        &book.path,
        &book.filename,
        book.cat_type,
        &config.library.zip_codepage,
    ) else {
        return Ok(());
    };
    let format = parts[2];
    let (data, target) = if format == "orig" {
        (original, book.format.clone())
    } else {
        match crate::convert::convert(&config.convert, &original, &book.filename, format).await {
            Ok(data) => (data, format.to_string()),
            Err(e) => {
                bot.send_message(chat_id, format!("❌ Ошибка конвертации: {e}"))
                    .await?;
                return Ok(());
            }
        }
    };
    let filename = crate::opds::download::title_to_filename(&book.title, &target, &book.filename);
    bot.send_document(chat_id, InputFile::memory(data).file_name(filename))
        .await?;
    Ok(())
}

fn allowed_message(msg: &Message, config: &Config) -> bool {
    allowed_username(
        msg.from.as_ref().and_then(|u| u.username.as_deref()),
        config,
    )
}

fn allowed_username(username: Option<&str>, config: &Config) -> bool {
    config.telegram.allowed_usernames.is_empty()
        || username.is_some_and(|name| {
            config
                .telegram
                .allowed_usernames
                .iter()
                .any(|v| v.eq_ignore_ascii_case(name))
        })
}

fn page_count(total: i64, page_size: i64) -> usize {
    if total <= 0 {
        0
    } else {
        ((total + page_size - 1) / page_size) as usize
    }
}

const BOOK_EMOJIS: [&str; 10] = ["📕", "📗", "📘", "📙", "📒", "📔", "📓", "📚", "📖", "🔖"];

const MALE_EMOJIS: [&str; 7] = ["👨", "🧔", "👱‍♂️", "👨‍🦱", "👨‍🦰", "👨‍🦳", "👴"];

const FEMALE_EMOJIS: [&str; 7] = ["👩", "👱‍♀️", "👩‍🦱", "👩‍🦰", "👩‍🦳", "👵", "🧕"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gender {
    Male,
    Female,
}

/// Cheap Russian author gender heuristic: patronymic suffix is decisive
/// (`-вич/-ич` male, `-вна/-чна/-шна` female), otherwise the given name's
/// last letter (`а`/`я` → female). Approximate by design — it only picks an emoji.
fn author_gender(full_name: &str) -> Gender {
    let clean = |w: &str| w.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase();
    for word in full_name.split_whitespace() {
        let w = clean(word);
        if w.ends_with("вна") || w.ends_with("чна") || w.ends_with("шна") {
            return Gender::Female;
        }
        if w.ends_with("вич") || w.ends_with("ич") || w.ends_with("ыч") {
            return Gender::Male;
        }
    }
    // Fallback: given name (second word in "Surname Given Patronymic").
    let words: Vec<&str> = full_name.split_whitespace().collect();
    let candidate = match words.len() {
        0 => return Gender::Male,
        1 => words[0],
        _ => words[1],
    };
    if let Some(last) = clean(candidate).chars().last() {
        if last == 'а' || last == 'я' {
            return Gender::Female;
        }
    }
    Gender::Male
}

/// Keep only the year out of an FB2/INPX date field (e.g. "1999", "1999-06-10",
/// "10.06.1999" → "1999"). Returns the first standalone 4-digit 1000..=2100 run.
fn extract_year(docdate: &str) -> String {
    let bytes = docdate.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4].iter().all(u8::is_ascii_digit) {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_digit();
            let after_ok = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_digit();
            if before_ok && after_ok {
                if let Ok(y) = docdate[i..i + 4].parse::<u32>() {
                    if (1000..=2100).contains(&y) {
                        return y.to_string();
                    }
                }
            }
        }
        i += 1;
    }
    docdate.trim().to_string()
}

/// Render a number with mathematical-bold digits for button labels, where Telegram
/// inline-button text cannot carry HTML `<b>` formatting.
fn bold_digits(n: usize) -> String {
    const BOLD: [char; 10] = ['𝟎', '𝟏', '𝟐', '𝟑', '𝟒', '𝟓', '𝟔', '𝟕', '𝟖', '𝟗'];
    n.to_string()
        .chars()
        .map(|c| c.to_digit(10).map_or(c, |d| BOLD[d as usize]))
        .collect()
}

/// Spaces to pad the author line so its emoji sits directly under the book emoji.
/// The book emoji is preceded by `"{number}. "` (e.g. "1. ", "47. ", "100. "), so the
/// indent grows with the number of digits — handles multi-digit result numbers.
fn author_line_indent(number: usize) -> String {
    " ".repeat(format!("{number}. ").chars().count())
}

fn format_emoji(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "fb2" => "📄",
        "epub" => "📕",
        "mobi" => "📘",
        "azw3" | "kfx" => "📙",
        "kepub" => "📗",
        "pdf" => "🧾",
        "txt" => "📝",
        "djvu" => "📚",
        "docx" => "📃",
        "zip" => "🗜️",
        _ => "⬇️",
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn strip_html(value: &str) -> String {
    Regex::new(r"<[^>]*>")
        .unwrap()
        .replace_all(value, "")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gender_from_patronymic() {
        assert_eq!(author_gender("Пелевин Виктор Олегович"), Gender::Male);
        assert_eq!(author_gender("Иванов Иван Иванович"), Gender::Male);
        assert_eq!(author_gender("Ахматова Анна Андреевна"), Gender::Female);
        assert_eq!(author_gender("Толстая Татьяна Никитична"), Gender::Female);
    }

    #[test]
    fn gender_fallback_by_given_name() {
        assert_eq!(author_gender("Ахматова Анна"), Gender::Female);
        assert_eq!(author_gender("Пелевин Виктор"), Gender::Male);
        assert_eq!(author_gender("Unknown"), Gender::Male);
    }

    #[test]
    fn year_extraction() {
        assert_eq!(extract_year("1999"), "1999");
        assert_eq!(extract_year("1999-06-10"), "1999");
        assert_eq!(extract_year("10.06.1999"), "1999");
        assert_eq!(extract_year(""), "");
        assert_eq!(extract_year("нет данных"), "нет данных");
    }

    #[test]
    fn bold_digit_rendering() {
        assert_eq!(bold_digits(1), "𝟏");
        assert_eq!(bold_digits(10), "𝟏𝟎");
        assert_eq!(bold_digits(47), "𝟒𝟕");
    }

    #[test]
    fn author_line_indent_width() {
        assert_eq!(author_line_indent(1), "   "); // "1. " = 3 chars
        assert_eq!(author_line_indent(47), "    "); // "47. " = 4 chars
        assert_eq!(author_line_indent(100), "     "); // "100. " = 5 chars
    }
}
