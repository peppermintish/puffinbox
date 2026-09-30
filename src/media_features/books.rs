//! Read-only PDF and EPUB access for indexed book items.
//!
//! EPUBs are delivered as bounded ZIP bytes to the bundled reader, which parses
//! the package client-side and constructs a small allow-listed document tree.
//! PDF bytes are served in ranges from the capability-opened catalog file and
//! rendered by bundled PDF.js with annotations, scripting, and XFA disabled.
//! EPUB bytes are never opened as a browser document; the reader builds a
//! constrained tree from allow-listed XHTML elements.

use std::{
    io,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::{Path as RoutePath, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
            IF_RANGE, RANGE,
        },
    },
    response::Response,
    routing::get,
};
use bytes::Bytes;
use futures_util::stream;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{ApiError, auth::MediaUser, db, library::ItemRecord, state::AppState};

const MAX_PDF_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EPUB_BYTES: u64 = 64 * 1024 * 1024;
const PDF_HEADER_SCAN_BYTES: usize = 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BookFormat {
    Pdf,
    Epub,
}

impl BookFormat {
    fn from_item(item: &ItemRecord) -> Option<Self> {
        if !is_readable_book_type(&item.item_type) {
            return None;
        }
        match item
            .path
            .extension()?
            .to_str()?
            .to_ascii_lowercase()
            .as_str()
        {
            "pdf" => Some(Self::Pdf),
            "epub" => Some(Self::Epub),
            _ => None,
        }
    }

    fn max_bytes(self) -> u64 {
        match self {
            Self::Pdf => MAX_PDF_BYTES,
            Self::Epub => MAX_EPUB_BYTES,
        }
    }
}

pub(super) fn router(state: AppState) -> Router {
    Router::new()
        .route("/Books/{item_id}/Reader", get(reader_page))
        .route(
            "/Books/{item_id}/Document",
            get(pdf_document).head(pdf_document),
        )
        .route(
            "/Books/{item_id}/Epub",
            get(epub_archive).head(epub_archive),
        )
        .route(
            "/Books/{item_id}/Download",
            get(download_book).head(download_book),
        )
        .with_state(state)
}

async fn reader_page(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    RoutePath(item_id): RoutePath<Uuid>,
) -> Result<Response, ApiError> {
    let (item, format, _) = open_book(&state, &user, item_id).await?;
    let html = reader_document(item.id, &item.name, format, user.enable_content_downloading);
    let html_len = html.len();
    let mut response = Response::new(Body::from(html));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(CONTENT_LENGTH, decimal_header(html_len as u64));
    apply_private_security_headers(headers);
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'unsafe-inline'; connect-src 'self'; worker-src 'self'; img-src 'self' data: blob:; base-uri 'none'; form-action 'none'; frame-ancestors 'self'",
        ),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("SAMEORIGIN"),
    );
    Ok(response)
}

async fn epub_archive(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    RoutePath(item_id): RoutePath<Uuid>,
    method: Method,
) -> Result<Response, ApiError> {
    let (_, format, opened) = open_book(&state, &user, item_id).await?;
    if format != BookFormat::Epub {
        return Err(ApiError::NotFound);
    }
    let mut file = tokio::fs::File::from_std(opened.file);
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|_| ApiError::Unavailable)?;

    let length = opened.size;
    let stream_permit = if method == Method::GET {
        Some(epub_stream_permit()?)
    } else {
        None
    };
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let stream = ReaderStream::with_capacity(file.take(length), STREAM_CHUNK_BYTES);
        Body::from_stream(stream.map_ok_with_permit(stream_permit.ok_or(ApiError::Unavailable)?))
    };
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/epub+zip"),
    );
    headers.insert(CONTENT_LENGTH, decimal_header(length));
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "sandbox; default-src 'none'; script-src 'none'; object-src 'none'; base-uri 'none'",
        ),
    );
    apply_private_security_headers(headers);
    Ok(response)
}

async fn pdf_document(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    RoutePath(item_id): RoutePath<Uuid>,
    method: Method,
    request_headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (_, format, opened) = open_book(&state, &user, item_id).await?;
    if format != BookFormat::Pdf {
        return Err(ApiError::NotFound);
    }

    let total_len = opened.size;
    let etag = media_etag(total_len, opened.modified);
    let mut selected_range = None;
    if let Some(raw_range) = request_headers.get(RANGE)
        && !request_headers.contains_key(IF_RANGE)
    {
        let raw_range = raw_range
            .to_str()
            .map_err(|_| ApiError::BadRequest("Invalid Range header".to_owned()))?;
        match super::range::parse_range_header(raw_range, total_len) {
            Ok(range) => selected_range = range,
            Err(_) => {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                response.headers_mut().insert(
                    CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{total_len}"))
                        .expect("generated content range is valid"),
                );
                let headers = response.headers_mut();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
                headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                headers.insert(ETAG, etag);
                apply_pdf_security_headers(headers);
                return Ok(response);
            }
        }
    }

    let (status, start, length) = match selected_range {
        Some(range) => (StatusCode::PARTIAL_CONTENT, range.start(), range.len()),
        None => (StatusCode::OK, 0, total_len),
    };
    let permit = if method == Method::GET {
        Some(pdf_stream_permit()?)
    } else {
        None
    };
    let mut file = tokio::fs::File::from_std(opened.file);
    file.seek(SeekFrom::Start(start))
        .await
        .map_err(|_| ApiError::Unavailable)?;
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let stream = ReaderStream::with_capacity(file.take(length), STREAM_CHUNK_BYTES);
        Body::from_stream(stream.map_ok_with_permit(permit.ok_or(ApiError::Unavailable)?))
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/pdf"));
    headers.insert(CONTENT_LENGTH, decimal_header(length));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(ETAG, etag);
    if let Some(range) = selected_range {
        headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                range.start(),
                range.end_inclusive(),
                total_len
            ))
            .expect("generated content range is valid"),
        );
    }
    apply_pdf_security_headers(headers);
    Ok(response)
}

async fn download_book(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    RoutePath(item_id): RoutePath<Uuid>,
    method: Method,
) -> Result<Response, ApiError> {
    if !user.enable_content_downloading {
        return Err(ApiError::Forbidden);
    }
    let (_, format, opened) = open_book(&state, &user, item_id).await?;
    let length = opened.size;
    let mut file = tokio::fs::File::from_std(opened.file);
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|_| ApiError::Unavailable)?;
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let permit = match format {
            BookFormat::Pdf => pdf_stream_permit()?,
            BookFormat::Epub => epub_stream_permit()?,
        };
        let stream = ReaderStream::with_capacity(file.take(length), STREAM_CHUNK_BYTES);
        Body::from_stream(stream.map_ok_with_permit(permit))
    };
    let (content_type, disposition) = match format {
        BookFormat::Pdf => ("application/pdf", "attachment; filename=\"book.pdf\""),
        BookFormat::Epub => ("application/epub+zip", "attachment; filename=\"book.epub\""),
    };
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(CONTENT_LENGTH, decimal_header(length));
    headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static(disposition));
    apply_private_security_headers(headers);
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "sandbox; default-src 'none'; script-src 'none'; object-src 'none'; base-uri 'none'",
        ),
    );
    Ok(response)
}

async fn open_book(
    state: &AppState,
    user: &crate::auth::UserRecord,
    item_id: Uuid,
) -> Result<(ItemRecord, BookFormat, super::secure_path::OpenedMedia), ApiError> {
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, user, &item).await? {
        return Err(ApiError::NotFound);
    }
    let format = BookFormat::from_item(&item).ok_or(ApiError::NotFound)?;
    let opened = super::open_authorized_item(state, user, item_id).await?;
    if opened.size == 0 || opened.size > format.max_bytes() {
        return Err(ApiError::NotFound);
    }
    if !has_expected_signature(format, &opened.file).await? {
        return Err(ApiError::NotFound);
    }
    Ok((item, format, opened))
}

async fn has_expected_signature(
    format: BookFormat,
    file: &std::fs::File,
) -> Result<bool, ApiError> {
    let mut file = tokio::fs::File::from_std(file.try_clone().map_err(|_| ApiError::Unavailable)?);
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|_| ApiError::Unavailable)?;
    match format {
        BookFormat::Pdf => {
            let mut header = vec![0; PDF_HEADER_SCAN_BYTES];
            let count = file
                .read(&mut header)
                .await
                .map_err(|_| ApiError::Unavailable)?;
            Ok(header[..count].windows(5).any(|window| window == b"%PDF-"))
        }
        BookFormat::Epub => {
            let mut header = [0; 4];
            if file.read_exact(&mut header).await.is_err() {
                return Ok(false);
            }
            Ok(header == [b'P', b'K', 3, 4])
        }
    }
}

fn reader_document(
    item_id: Uuid,
    item_name: &str,
    format: BookFormat,
    can_download: bool,
) -> String {
    let item_id = item_id.to_string();
    let item_name = escape_html_attribute(item_name);
    let (label, format_attr) = match format {
        BookFormat::Pdf => ("PDF", "pdf"),
        BookFormat::Epub => ("EPUB", "epub"),
    };
    let download_hidden = if can_download { "" } else { " hidden" };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"color-scheme\" content=\"light dark\"><title>Puffinbox book reader</title><style>{}</style><script src=\"/web/book-reader.js\" defer></script></head><body><main id=\"book-reader\" data-item-id=\"{item_id}\" data-book-title=\"{item_name}\" data-format=\"{format_attr}\"><header class=\"reader-header\"><a class=\"back-link\" href=\"/\">Puffinbox</a><div class=\"book-heading\"><h1 id=\"book-title\">{item_name}</h1><p id=\"reader-status\" role=\"status\" aria-live=\"polite\">Opening {label}…</p></div><a id=\"download-link\" class=\"download-link\" href=\"/Books/{item_id}/Download\"{download_hidden}>Download</a></header><section id=\"pdf-panel\" class=\"pdf-panel\" hidden><nav class=\"pdf-controls\" aria-label=\"PDF pages\"><button id=\"previous-page\" type=\"button\">Previous page</button><span id=\"pdf-page-label\" aria-live=\"polite\"></span><button id=\"next-page\" type=\"button\">Next page</button></nav><div class=\"pdf-page-area\"><canvas id=\"pdf-canvas\" aria-label=\"PDF page\"></canvas></div></section><section id=\"epub-panel\" class=\"epub-panel\" hidden><nav class=\"chapter-controls\" aria-label=\"Book chapters\"><button id=\"previous-chapter\" type=\"button\">Previous</button><label>Chapter <select id=\"chapter-list\"></select></label><button id=\"next-chapter\" type=\"button\">Next</button></nav><article id=\"epub-content\" class=\"epub-content\" tabindex=\"-1\"></article></section><p id=\"reader-error\" class=\"reader-error\" role=\"alert\" hidden></p><noscript>This reader requires JavaScript to safely display EPUB content.</noscript></main></body></html>",
        READER_CSS
    )
}

fn escape_html_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars().take(256) {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character if character.is_control() => escaped.push('�'),
            character => escaped.push(character),
        }
    }
    escaped
}

const READER_CSS: &str = r#"
:root{color-scheme:light dark;font:16px/1.55 system-ui,sans-serif;background:#11151f;color:#e8eaf0}
*{box-sizing:border-box}body{margin:0;min-height:100vh}.reader-header{position:sticky;top:0;z-index:2;display:flex;align-items:center;gap:1rem;padding:.75rem clamp(1rem,4vw,3rem);background:#171c28;border-bottom:1px solid #343c4c}.back-link,.download-link{color:#b7c7ff;text-decoration:none}.book-heading{min-width:0;flex:1}.book-heading h1{font-size:1.05rem;margin:0;overflow-wrap:anywhere}.book-heading p{font-size:.85rem;color:#b8bfce;margin:.1rem 0 0}.download-link{white-space:nowrap}.pdf-panel{min-height:calc(100vh - 72px);padding:0 clamp(.5rem,3vw,2rem)}.pdf-controls,.chapter-controls{position:sticky;top:72px;z-index:1;display:flex;align-items:center;justify-content:space-between;gap:.75rem;padding:.7rem;background:#202636;border:1px solid #343c4c;border-radius:.75rem}.pdf-controls button,.chapter-controls button,.chapter-controls select{font:inherit;color:inherit;background:#171c28;border:1px solid #586176;border-radius:.4rem;padding:.4rem .65rem}.pdf-controls button:disabled,.chapter-controls button:disabled{opacity:.5}.pdf-page-area{display:flex;justify-content:center;padding:1rem 0 3rem}.pdf-page-area canvas{display:block;max-width:100%;height:auto;background:#fff;box-shadow:0 1rem 3rem #0006}.epub-panel{max-width:62rem;margin:1rem auto;padding:0 1rem 4rem}.chapter-controls label{display:flex;align-items:center;gap:.5rem}.epub-content{margin:1.5rem auto 0;max-width:44rem;padding:clamp(1rem,5vw,3.5rem);background:#f8f5ed;color:#202127;border-radius:.5rem;box-shadow:0 1rem 3rem #0004;overflow-wrap:anywhere}.epub-content a{color:#244b9a}.epub-content pre{white-space:pre-wrap;overflow-wrap:anywhere}.epub-content img,.epub-content svg,.epub-content iframe,.epub-content video,.epub-content audio{display:none!important}.reader-error{margin:1rem auto;max-width:48rem;padding:1rem;background:#501d27;color:#fff;border-radius:.5rem}button:focus-visible,a:focus-visible,select:focus-visible,.epub-content:focus-visible{outline:3px solid #93aaff;outline-offset:2px}noscript{display:block;margin:1rem}
"#;

fn is_readable_book_type(item_type: &str) -> bool {
    matches!(item_type.to_ascii_lowercase().as_str(), "book" | "ebook")
}

fn apply_private_security_headers(headers: &mut HeaderMap) {
    headers.insert(
        HeaderName::from_static("cache-control"),
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
}

fn apply_pdf_security_headers(headers: &mut HeaderMap) {
    apply_private_security_headers(headers);
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "sandbox; default-src 'none'; script-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'",
        ),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("SAMEORIGIN"),
    );
}

fn decimal_header(value: u64) -> HeaderValue {
    HeaderValue::from_str(&value.to_string()).expect("decimal content length is valid")
}

fn media_etag(size: u64, modified: Option<SystemTime>) -> HeaderValue {
    let (seconds, nanos) = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or_default();
    HeaderValue::from_str(&format!("W/\"book-{size}-{seconds}-{nanos}\""))
        .expect("generated ETag is valid")
}

fn epub_stream_permit() -> Result<OwnedSemaphorePermit, ApiError> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(8)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)
}

fn pdf_stream_permit() -> Result<OwnedSemaphorePermit, ApiError> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(16)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)
}

trait StreamPermitExt: Sized {
    fn map_ok_with_permit(
        self,
        permit: OwnedSemaphorePermit,
    ) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send;
}

impl<S> StreamPermitExt for S
where
    S: futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + Unpin + 'static,
{
    fn map_ok_with_permit(
        self,
        permit: OwnedSemaphorePermit,
    ) -> impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send {
        stream::unfold((self, Some(permit)), |(mut source, permit)| async move {
            use futures_util::StreamExt;
            match source.next().await {
                Some(chunk) => Some((chunk, (source, permit))),
                None => {
                    drop(permit);
                    None
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BookFormat, MAX_EPUB_BYTES, MAX_PDF_BYTES, is_readable_book_type, media_etag,
        reader_document,
    };
    use crate::library::ItemRecord;
    use chrono::Utc;
    use std::{path::PathBuf, time::UNIX_EPOCH};
    use uuid::Uuid;

    #[test]
    fn readable_book_types_are_restricted_to_pdf_and_epub_book_items() {
        for allowed in ["Book", "EBook", "ebook"] {
            assert!(is_readable_book_type(allowed));
        }
        for denied in ["AudioBook", "Movie", "File", "Folder"] {
            assert!(!is_readable_book_type(denied));
        }
        assert_eq!(MAX_PDF_BYTES, 256 * 1024 * 1024);
        assert_eq!(MAX_EPUB_BYTES, 64 * 1024 * 1024);
    }

    #[test]
    fn format_selection_uses_book_type_and_case_insensitive_extension() {
        let mut item = fixture("/library/My book.PDF", "Book");
        assert_eq!(BookFormat::from_item(&item), Some(BookFormat::Pdf));
        item.path = PathBuf::from("/library/My book.epub");
        item.item_type = "EBook".to_owned();
        assert_eq!(BookFormat::from_item(&item), Some(BookFormat::Epub));
        item.path = PathBuf::from("/library/My book.html");
        assert_eq!(BookFormat::from_item(&item), None);
        item.path = PathBuf::from("/library/book.pdf");
        item.item_type = "AudioBook".to_owned();
        assert_eq!(BookFormat::from_item(&item), None);
    }

    #[test]
    fn reader_page_contains_only_server_selected_format_and_valid_uuid() {
        let id = Uuid::new_v4();
        let html = reader_document(id, "book \"title\" <safe>", BookFormat::Epub, false);
        assert!(html.contains(&format!("data-item-id=\"{id}\"")));
        assert!(html.contains("data-format=\"epub\""));
        assert!(html.contains("data-book-title=\"book &quot;title&quot; &lt;safe&gt;\""));
        assert!(html.contains("/web/book-reader.js"));
        assert!(html.contains("/Books/") && html.contains("/Download\" hidden"));
        assert!(!html.contains("innerHTML"));
        assert!(!html.contains("script-src 'unsafe-inline'"));
    }

    #[test]
    fn etag_is_weak_and_contains_only_safe_metadata() {
        let etag = media_etag(42, Some(UNIX_EPOCH));
        assert_eq!(etag.to_str().unwrap(), "W/\"book-42-0-0\"");
    }

    fn fixture(path: &str, item_type: &str) -> ItemRecord {
        ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "My book".to_owned(),
            sort_name: "my book".to_owned(),
            item_type: item_type.to_owned(),
            path: PathBuf::from(path),
            container: None,
            size_bytes: Some(4),
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::json!({}),
        }
    }
}
