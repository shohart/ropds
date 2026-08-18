use std::sync::Arc;
use std::time::Duration;

use teloxide::dispatching::UpdateFilterExt;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::DbPool;
use crate::db::queries::{authors, books};

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
    info!("Telegram bot started");

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(handle_message))
        .branch(Update::filter_callback_query().endpoint(handle_callback));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![pool, config])
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
            "📚 <b>ROPDS — домашняя библиотека</b>\n\n🔎 Отправьте часть названия книги или имени автора.\n⬇️ Затем выберите книгу и нужный формат.",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }
    if let Some(id) = text
        .strip_prefix("/download")
        .and_then(|v| v.parse::<i64>().ok())
    {
        send_book_card(&bot, msg.chat.id, &pool, &config, id).await?;
        return Ok(());
    }
    if text.chars().count() < 3 {
        bot.send_message(msg.chat.id, "⚠️ Для поиска нужно минимум 3 символа.")
            .await?;
        return Ok(());
    }

    let found = books::search_title_or_author(&pool, text, config.telegram.max_results as i64)
        .await
        .unwrap_or_default();
    if found.is_empty() {
        bot.send_message(
            msg.chat.id,
            "❌ Ничего не найдено. Попробуйте другой запрос.",
        )
        .await?;
        return Ok(());
    }
    let mut output = format!("✅ <b>Найдено: {}</b>\n\n", found.len());
    for book in found {
        let names = authors::get_for_book(&pool, book.id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.full_name)
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "📖 <b>{}</b>\n✍️ {}\n⬇️ <code>/download{}</code>\n\n",
            escape_html(&book.title),
            escape_html(if names.is_empty() { "—" } else { &names }),
            book.id
        ));
    }
    bot.send_message(msg.chat.id, output)
        .parse_mode(ParseMode::Html)
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
    let mut buttons = vec![InlineKeyboardButton::callback(
        format!("📄 {}", book.format.to_uppercase()),
        format!("get:{id}:orig"),
    )];
    if book.format == "fb2" && config.convert.enabled {
        for format in &config.convert.formats {
            buttons.push(InlineKeyboardButton::callback(
                format!("⬇️ {}", format.to_uppercase()),
                format!("get:{id}:{format}"),
            ));
        }
    }
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
        .reply_markup(InlineKeyboardMarkup::new([buttons]))
        .await?;
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    pool: DbPool,
    config: Arc<Config>,
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
                if let Some(message) = query.message {
                    bot.send_message(message.chat().id, format!("❌ Ошибка конвертации: {e}"))
                        .await?;
                }
                return Ok(());
            }
        }
    };
    if let Some(message) = query.message {
        let filename =
            crate::opds::download::title_to_filename(&book.title, &target, &book.filename);
        bot.send_document(
            message.chat().id,
            InputFile::memory(data).file_name(filename),
        )
        .await?;
    }
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

use regex::Regex;
