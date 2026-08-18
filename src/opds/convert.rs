use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::db::queries::{books, bookshelf};
use crate::opds::download::{file_response, read_book_file, title_to_filename};
use crate::opds::v1::xml;
use crate::state::AppState;

use super::auth;

/// GET /opds/convert/:book_id/:format/  (format = `epub` | `mobi`)
///
/// Converts an FB2 book on the fly using the configured external converter and
/// returns the result as a download. Non-FB2 books are rejected.
pub async fn convert(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((book_id, format)): Path<(i64, String)>,
) -> Response {
    if !state.config.convert.enabled {
        return (StatusCode::SERVICE_UNAVAILABLE, "conversion disabled").into_response();
    }

    let target = format.to_ascii_lowercase();
    if !matches!(target.as_str(), "epub" | "mobi") {
        return (StatusCode::BAD_REQUEST, "unsupported target format").into_response();
    }

    let book = match books::get_by_id(&state.db, book_id).await {
        Ok(Some(b)) => b,
        Ok(None) => return (StatusCode::NOT_FOUND, "book not found").into_response(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "db error").into_response(),
    };

    if book.format != "fb2" {
        return (
            StatusCode::BAD_REQUEST,
            "conversion is only available for fb2 books",
        )
            .into_response();
    }

    let data = match read_book_file(
        &state.config.library.root_path,
        &book.path,
        &book.filename,
        book.cat_type,
        &state.config.library.zip_codepage,
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("failed to read book {book_id} for conversion: {e}");
            return (StatusCode::NOT_FOUND, "file not found").into_response();
        }
    };

    // Track bookshelf, mirroring the download endpoint behaviour.
    if let Some(user_id) = auth::get_user_id_from_headers(&state.db, &headers).await {
        let _ = bookshelf::upsert(&state.db, user_id, book_id).await;
    }

    let converted = match crate::convert::convert(
        &state.config.convert,
        &data,
        &book.filename,
        &target,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("conversion failed for book {book_id} → {target}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("conversion failed: {e}"),
            )
                .into_response();
        }
    };

    let filename = title_to_filename(&book.title, &target, &book.filename);
    let mime = xml::mime_for_format(&target);
    file_response(&converted, &filename, mime)
}
