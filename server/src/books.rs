//! Books: paste/PDF ingestion via flick-core, timelines, reading position.

use axum::extract::multipart::MultipartError;
use axum::extract::{FromRequest, Multipart, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use flick_core::Timeline;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{random_token, AuthUser};
use crate::config::{Config, Edition};
use crate::db::{self, now_secs, Book};
use crate::error::{AppError, AppJson, AppPath};
use crate::import::{self, Prepared};
use crate::AppState;

/// Upload/body cap (CONTRACTS.md: 25 MB).
pub const UPLOAD_LIMIT: usize = 25 * 1024 * 1024;

/// Hosted free plan: user-sourced uploads allowed per ISO week
/// (CONTRACTS.md "Editions & plans").
pub const WEEKLY_UPLOAD_LIMIT: i64 = 15;

/// The user's weekly upload allowance — `Some(15)` only on the hosted
/// edition's free plan (guests included); `None` = unlimited (selfhost
/// edition, or pro).
pub fn weekly_upload_limit(config: &Config, user: &db::User) -> Option<i64> {
    (config.edition == Edition::Hosted && user.plan == "free").then_some(WEEKLY_UPLOAD_LIMIT)
}

const TITLE_CHARS: usize = 40;

/// Default title: first ~40 chars of the text (whitespace collapsed).
fn default_title(text: &str) -> String {
    let mut title = String::new();
    for word in text.split_whitespace() {
        if !title.is_empty() {
            if title.chars().count() + 1 + word.chars().count() > TITLE_CHARS {
                break;
            }
            title.push(' ');
        }
        title.push_str(word);
        if title.chars().count() >= TITLE_CHARS {
            title = title.chars().take(TITLE_CHARS).collect();
            break;
        }
    }
    if title.is_empty() {
        "Untitled".into()
    } else {
        title
    }
}

fn clean_title(title: Option<String>) -> Option<String> {
    title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

// ------------------------------------------------------------ starter book

const INTRO_TITLE: &str = "Welcome to flick";

const INTRO_TEXT: &str = "\
Welcome to flick.

You are reading with your eye locked to one spot. The red letter is the anchor. \
Words come to you, not the other way around. This is how you read faster.

Press space to pause and resume. Tap the left or right side of the reader, or use \
the arrow keys, to jump back or forward one sentence. The slider changes your \
speed in words per minute.

Start around 300. When that feels easy, go up by 50. Speed reading is a skill: at \
first 400 feels fast, then it feels slow. Most people double their pace within a \
week of practice.

Notice how long words get a little more time. So do sentence endings, like this \
one. That rhythm is computed for every book you add, so the pace always feels \
natural.

Add your own books with paste or PDF upload. Your reading position follows your \
account across devices. You can delete this book whenever you are done with it.

Happy flicking.";

/// Seed the built-in intro book for a freshly created user (contract:
/// every new non-guest user starts with one; `source: \"intro\"`).
pub fn seed_intro_book(c: &rusqlite::Connection, user_id: &str, now: i64) -> rusqlite::Result<()> {
    let timeline = Timeline::from_text(INTRO_TEXT);
    let timeline_json = serde_json::to_vec(&timeline).map_err(|e| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(e))
    })?;
    let book = Book {
        id: random_token(16),
        title: INTRO_TITLE.into(),
        source: "intro".into(),
        word_count: timeline.word_count as i64,
        position: 0,
        created_at: now,
        last_read_at: None,
        author: None,
        url: None,
        favicon: None,
        excerpt: None,
        category: None,
        tags: Vec::new(),
    };
    db::insert_book(c, user_id, &book, &timeline_json, Some(INTRO_TEXT), None)
}

/// Backstop for pre-v0.4.1 guest rows that are still empty: guests are now
/// seeded with the full starter library at creation (contract "Starter
/// library"), but an old empty guest gets it alongside their first own add
/// instead. Call before inserting the user's new book.
pub fn maybe_seed_guest_defaults(
    c: &rusqlite::Connection,
    user: &db::User,
    now: i64,
) -> rusqlite::Result<()> {
    if user.guest && db::book_count(c, &user.id)? == 0 {
        crate::catalog::seed_default_library(c, &user.id, now)?;
    }
    Ok(())
}

// --------------------------------------------------------------- handlers

#[derive(Deserialize)]
pub struct ListQuery {
    q: Option<String>,
}

/// Build a safe FTS5 MATCH query from user input: each whitespace-run becomes a
/// quoted term (alphanumerics + apostrophes only, so no FTS operator can leak
/// in), ANDed together. `None` when nothing searchable remains — the caller
/// then returns an empty list rather than a 500 (CONTRACTS.md: FTS syntax
/// errors must not surface as 500s).
fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// GET /api/books — the whole library, or an FTS5 search when `?q=` is given.
pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<Book>>, AppError> {
    let books = match query.q {
        Some(q) if !q.trim().is_empty() => {
            let Some(match_query) = fts_query(&q) else {
                return Ok(Json(Vec::new()));
            };
            state
                .db
                .call(move |c| db::search_books(c, &user.id, &match_query))
                .await
                // A malformed MATCH is the client's problem, never a 500.
                .map_err(|_| AppError::bad_request("invalid search query"))?
        }
        _ => {
            state
                .db
                .call(move |c| db::list_books(c, &user.id))
                .await?
        }
    };
    Ok(Json(books))
}

#[derive(Deserialize)]
pub struct PasteBody {
    title: Option<String>,
    text: String,
}

fn multipart_err(e: MultipartError) -> AppError {
    AppError::Status(e.status(), e.body_text())
}

/// Tokenize a `Prepared` book through flick-core (off the async runtime),
/// persist it (with its plaintext, for `/text` + search), and return `201`.
/// Shared by paste, uploads and both web-import paths. `title_fallback` fills
/// in a title when the parser found none (e.g. paste/pdf → first ~40 chars).
async fn insert_prepared(
    state: &AppState,
    user: &crate::db::User,
    prepared: Prepared,
) -> Result<Response, AppError> {
    let Prepared {
        title,
        text,
        source,
        author,
        url,
        favicon,
        excerpt,
        category,
    } = prepared;

    if text.trim().is_empty() {
        return Err(AppError::bad_request("no readable text was found"));
    }
    let title = clean_title(title).unwrap_or_else(|| default_title(&text));

    // Timeline construction can chew through megabytes of text; keep it off
    // the async runtime. flick-core is the only tokenizer.
    let (timeline_json, word_count, text) = tokio::task::spawn_blocking(move || {
        let timeline = Timeline::from_text(&text);
        serde_json::to_vec(&timeline).map(|json| (json, timeline.word_count, text))
    })
    .await
    .map_err(AppError::internal)?
    .map_err(AppError::internal)?;

    if word_count == 0 {
        return Err(AppError::bad_request("text contains no words"));
    }

    let book = Book {
        id: random_token(16),
        title,
        source: source.into(),
        word_count: word_count as i64,
        position: 0,
        created_at: now_secs(),
        last_read_at: None,
        author,
        url,
        favicon,
        excerpt,
        // Auto-tag with the category so mass imports arrive pre-sorted
        // (contract v0.4.3); users refine via PUT /tags.
        tags: category.iter().cloned().collect(),
        category,
    };
    // Weekly limit (hosted free plan only): counted and enforced inside the
    // insert closure so check + insert are atomic under the one connection.
    let limit = weekly_upload_limit(&state.config, user);
    let stored = book.clone();
    let user = user.clone();
    let inserted = state
        .db
        .call(move |c| {
            if let Some(limit) = limit {
                if db::uploads_this_week(c, &user.id, stored.created_at)? >= limit {
                    return Ok(false);
                }
            }
            maybe_seed_guest_defaults(c, &user, stored.created_at)?;
            db::insert_book(c, &user.id, &stored, &timeline_json, Some(&text), None)?;
            Ok(true)
        })
        .await?;
    if !inserted {
        return Err(AppError::Coded(
            StatusCode::FORBIDDEN,
            format!(
                "you've reached the free plan's {WEEKLY_UPLOAD_LIMIT} uploads for this week — \
                 the counter resets Monday (UTC), and everything already in your library stays"
            ),
            "upload_limit",
        ));
    }
    Ok((StatusCode::CREATED, Json(book)).into_response())
}

/// POST /api/books — JSON `{title?, text}` or a multipart `file` upload sniffed
/// by its bytes (PDF / EPUB / Kindle clippings / plain text; CONTRACTS.md).
pub async fn create(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    req: Request,
) -> Result<Response, AppError> {
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let prepared = if content_type.starts_with("multipart/form-data") {
        from_upload(&state, req).await?
    } else {
        let AppJson(body) = AppJson::<PasteBody>::from_request(req, &state).await?;
        if body.text.trim().is_empty() {
            return Err(AppError::bad_request("text must not be empty"));
        }
        Prepared {
            title: clean_title(body.title),
            text: body.text,
            source: "paste",
            author: None,
            url: None,
            favicon: None,
            excerpt: None,
            category: None,
        }
    };

    insert_prepared(&state, &user, prepared).await
}

/// Pull `title` + `file` out of the multipart body and route by content sniff.
async fn from_upload(state: &AppState, req: Request) -> Result<Prepared, AppError> {
    let mut multipart = Multipart::from_request(req, state)
        .await
        .map_err(|e| AppError::Status(e.status(), e.body_text()))?;

    let mut title: Option<String> = None;
    let mut file: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    while let Some(field) = multipart.next_field().await.map_err(multipart_err)? {
        match field.name().unwrap_or("") {
            "title" => title = Some(field.text().await.map_err(multipart_err)?),
            "file" => {
                filename = field.file_name().map(str::to_string);
                file = Some(field.bytes().await.map_err(multipart_err)?.to_vec());
            }
            _ => {
                // Drain unknown fields so the stream keeps advancing.
                field.bytes().await.map_err(multipart_err)?;
            }
        }
    }
    let file = file.ok_or_else(|| AppError::bad_request("missing multipart field \"file\""))?;
    let title = clean_title(title);
    sniff_bytes(file, title, filename.as_deref()).await
}

/// Route uploaded bytes to the right parser (bytes decide, not the extension).
/// The web-import fetch path reuses this for pdf/epub/plain content.
async fn sniff_bytes(
    bytes: Vec<u8>,
    title: Option<String>,
    filename: Option<&str>,
) -> Result<Prepared, AppError> {
    if import::looks_like_pdf(&bytes) {
        return from_pdf(bytes, title, filename).await;
    }
    if import::looks_like_epub(&bytes) {
        let mut prepared = import::extract_epub(bytes).await?;
        // An explicit upload title wins over EPUB metadata.
        if title.is_some() {
            prepared.title = title;
        }
        return Ok(prepared);
    }
    // Text formats require valid UTF-8.
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(AppError::bad_request(
            "unsupported file type (expected PDF, EPUB, or UTF-8 text)",
        ));
    };
    if let Some(mut prepared) = import::parse_clippings(&text) {
        if title.is_some() {
            prepared.title = title;
        }
        return Ok(prepared);
    }
    if text.trim().is_empty() {
        return Err(AppError::bad_request("the file has no readable text"));
    }
    let title = title.or_else(|| {
        filename
            .map(|f| {
                f.trim_end_matches(".txt")
                    .trim_end_matches(".md")
                    .trim()
                    .to_string()
            })
            .filter(|f| !f.is_empty())
    });
    Ok(Prepared {
        title,
        text,
        source: "txt",
        author: None,
        url: None,
        favicon: None,
        excerpt: None,
        category: Some("docs".into()),
    })
}

/// PDF text extraction, panic-guarded like the other parsers.
async fn from_pdf(
    file: Vec<u8>,
    title: Option<String>,
    filename: Option<&str>,
) -> Result<Prepared, AppError> {
    let extracted = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&file))
    })
    .await
    .map_err(AppError::internal)?;
    let text = match extracted {
        Ok(Ok(text)) if !text.trim().is_empty() => text,
        _ => {
            return Err(AppError::bad_request(
                "could not extract text from this PDF",
            ))
        }
    };
    let title = title.or_else(|| {
        filename
            .map(|f| f.trim_end_matches(".pdf").trim().to_string())
            .filter(|f| !f.is_empty())
    });
    Ok(Prepared {
        title,
        text,
        source: "pdf",
        author: None,
        url: None,
        favicon: None,
        excerpt: None,
        category: Some("docs".into()),
    })
}

#[derive(Deserialize)]
pub struct ImportUrlBody {
    url: String,
    title: Option<String>,
}

/// POST /api/import/url — server fetches the page (SSRF-guarded) and imports
/// it: pdf/epub/plain bytes go through the upload parsers, HTML through
/// readability (CONTRACTS.md).
pub async fn import_url(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppJson(body): AppJson<ImportUrlBody>,
) -> Result<Response, AppError> {
    let title = clean_title(body.title);
    let (final_url, bytes, content_type) = import::guarded_fetch(body.url.trim()).await?;

    let prepared = if import::looks_like_pdf(&bytes)
        || import::looks_like_epub(&bytes)
        || !import::looks_like_html(&content_type, &bytes)
    {
        // A concrete file (or plain text) behind the link — the Dropbox /
        // Google Picker direct-download path. Sniff it like an upload, then
        // re-tag it as a URL import and attach web metadata.
        let mut prepared = sniff_bytes(bytes, title, None).await?;
        prepared.source = "url";
        prepared.url = Some(final_url.clone());
        prepared.favicon = import::origin_favicon(&final_url);
        prepared
    } else {
        let html = String::from_utf8_lossy(&bytes).into_owned();
        let mut prepared = import::extract_article(html, final_url, "url").await?;
        if title.is_some() {
            prepared.title = title;
        }
        prepared
    };

    insert_prepared(&state, &user, prepared).await
}

#[derive(Deserialize)]
pub struct ImportHtmlBody {
    url: String,
    html: String,
    title: Option<String>,
}

/// POST /api/import/html — the extension path: readability runs on HTML the
/// client already captured (no server fetch, so no SSRF concern), for
/// paywalled/logged-in pages the user can see (CONTRACTS.md).
pub async fn import_html(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppJson(body): AppJson<ImportHtmlBody>,
) -> Result<Response, AppError> {
    if body.html.len() > import::IMPORT_LIMIT {
        return Err(AppError::bad_request("html exceeds the 25 MB limit"));
    }
    let url = body.url.trim().to_string();
    if url.is_empty() {
        return Err(AppError::bad_request("url must not be empty"));
    }
    let title = clean_title(body.title);
    let mut prepared = import::extract_article(body.html, url, "html").await?;
    if title.is_some() {
        prepared.title = title;
    }
    insert_prepared(&state, &user, prepared).await
}

pub async fn get_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<Json<Book>, AppError> {
    let book = state
        .db
        .call(move |c| db::get_book(c, &user.id, &id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(book))
}

pub async fn timeline(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<Response, AppError> {
    let blob = state
        .db
        .call(move |c| db::get_timeline(c, &user.id, &id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        blob,
    )
        .into_response())
}

/// GET /api/books/:id/text — the book as paragraphs of words, whose flattened
/// order/count matches the timeline exactly (same flick-core tokenizer), so
/// clients map the full-text view onto timeline indices 1:1. Books stored
/// before v0.3b (no plaintext) `404` here (contract: still readable, no /text).
pub async fn text(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<Json<Value>, AppError> {
    let text = state
        .db
        .call(move |c| db::book_text(c, &user.id, &id))
        .await?
        .ok_or(AppError::NotFound)?;
    let paragraphs = flick_core::paragraphs(&text);
    Ok(Json(json!({ "paragraphs": paragraphs })))
}

#[derive(Deserialize)]
pub struct PositionBody {
    position: i64,
    /// Words consumed since the client's last report (stats, optional).
    read: Option<i64>,
    /// The client's LOCAL date `YYYY-MM-DD` (streaks are a human-day
    /// concept); missing → server UTC date.
    day: Option<String>,
}

pub async fn set_position(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
    AppJson(body): AppJson<PositionBody>,
) -> Result<StatusCode, AppError> {
    if body.position < 0 {
        return Err(AppError::bad_request("position must be >= 0"));
    }
    let day = match body.day {
        Some(day) => {
            let Some(epoch_days) = crate::stats::parse_day(&day) else {
                return Err(AppError::bad_request("day must be a valid YYYY-MM-DD date"));
            };
            // Clock-abuse guard: reject days too far from the server date.
            if (epoch_days - crate::stats::today_epoch_days()).abs()
                > crate::stats::MAX_DAY_SKEW_DAYS
            {
                return Err(AppError::bad_request(
                    "day is too far from the server date",
                ));
            }
            day
        }
        None => crate::stats::utc_day(0),
    };
    let read = body
        .read
        .unwrap_or(0)
        .clamp(0, crate::stats::MAX_READ_PER_REPORT);
    enum Outcome {
        Missing,
        OutOfRange,
        Updated,
    }
    let position = body.position;
    let now = now_secs();
    let outcome = state
        .db
        .call(move |c| {
            let Some(book) = db::get_book(c, &user.id, &id)? else {
                return Ok(Outcome::Missing);
            };
            if position > book.word_count {
                return Ok(Outcome::OutOfRange);
            }
            db::set_position(c, &user.id, &id, position, now)?;
            if read > 0 {
                db::add_read_words(c, &user.id, &day, read)?;
            }
            Ok(Outcome::Updated)
        })
        .await?;
    match outcome {
        Outcome::Missing => Err(AppError::NotFound),
        Outcome::OutOfRange => Err(AppError::bad_request("position exceeds book length")),
        Outcome::Updated => Ok(StatusCode::NO_CONTENT),
    }
}

/// The auto-purge cutoff for now: everything trashed longer ago than the
/// retention window gets dropped whenever the trash is touched.
fn purge_cutoff(now: i64) -> i64 {
    now - db::TRASH_RETENTION_DAYS * 86_400
}

/// DELETE /api/books/{id} — soft delete: the book moves to the trash
/// (contract v0.4.3), restorable for TRASH_RETENTION_DAYS.
pub async fn delete_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let now = now_secs();
    let trashed = state
        .db
        .call(move |c| {
            db::purge_expired(c, &user.id, purge_cutoff(now))?;
            db::trash_book(c, &user.id, &id, now)
        })
        .await?;
    if trashed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// GET /api/books/trash — trashed books, newest first, with their expiry.
pub async fn list_trash(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>, AppError> {
    let now = now_secs();
    let rows = state
        .db
        .call(move |c| {
            db::purge_expired(c, &user.id, purge_cutoff(now))?;
            db::list_trash(c, &user.id)
        })
        .await?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|(b, deleted_at)| {
            json!({
                "id": b.id,
                "title": b.title,
                "author": b.author,
                "word_count": b.word_count,
                "deleted_at": deleted_at,
                "expires_at": deleted_at + db::TRASH_RETENTION_DAYS * 86_400,
            })
        })
        .collect();
    Ok(Json(json!({
        "items": items,
        "retention_days": db::TRASH_RETENTION_DAYS,
    })))
}

/// POST /api/books/{id}/restore — bring a trashed book back.
pub async fn restore_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let restored = state
        .db
        .call(move |c| db::restore_book(c, &user.id, &id))
        .await?;
    if restored {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// DELETE /api/books/{id}/purge — hard-delete a trashed book right away.
pub async fn purge_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let purged = state
        .db
        .call(move |c| db::purge_book(c, &user.id, &id))
        .await?;
    if purged {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

// ---------------------------------------------------------- share links

/// POST /api/books/{id}/share — mint (or return the existing) public share
/// token for a live book (contract v0.6). Idempotent.
pub async fn share_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<Json<Value>, AppError> {
    let fresh = random_token(12);
    let token = state
        .db
        .call(move |c| db::ensure_share_token(c, &user.id, &id, &fresh))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({ "token": token, "path": format!("/s/{token}") })))
}

/// DELETE /api/books/{id}/share — revoke the share link (204/404).
pub async fn unshare_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let cleared = state
        .db
        .call(move |c| db::clear_share_token(c, &user.id, &id))
        .await?;
    if cleared {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// GET /api/shared/{token} — public preview of a shared book (no auth).
pub async fn shared_info(
    State(state): State<AppState>,
    AppPath(token): AppPath<String>,
) -> Result<Json<Value>, AppError> {
    let (_, book) = state
        .db
        .call(move |c| db::book_by_share_token(c, &token))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "title": book.title,
        "author": book.author,
        "word_count": book.word_count,
        "category": book.category,
    })))
}

/// POST /api/shared/{token}/import — copy a shared book into the caller's
/// library (`source: "shared"`, never counts toward upload limits). A copy
/// that already exists (same share origin, matched by title+word_count) is
/// simply returned again.
pub async fn shared_import(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(token): AppPath<String>,
) -> Result<Response, AppError> {
    let now = now_secs();
    let book = state
        .db
        .call(move |c| {
            let Some((owner_id, src)) = db::book_by_share_token(c, &token)? else {
                return Ok(None);
            };
            let timeline = db::get_timeline(c, &owner_id, &src.id)?
                .expect("shared book has a timeline");
            let text = db::book_text(c, &owner_id, &src.id)?;
            let copy = Book {
                id: random_token(16),
                title: src.title.clone(),
                source: "shared".into(),
                word_count: src.word_count,
                position: 0,
                created_at: now,
                last_read_at: None,
                author: src.author.clone(),
                url: src.url.clone(),
                favicon: None,
                excerpt: src.excerpt.clone(),
                category: src.category.clone(),
                tags: src.tags.clone(),
            };
            db::insert_book(c, &user.id, &copy, &timeline, text.as_deref(), None)?;
            Ok(Some(copy))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((StatusCode::CREATED, Json(book)).into_response())
}

#[derive(Deserialize)]
pub struct TagsBody {
    tags: Vec<String>,
}

const MAX_TAGS: usize = 12;
const MAX_TAG_CHARS: usize = 24;

/// PUT /api/books/{id}/tags — replace a book's tags (≤12 tags, each 1–24
/// chars, trimmed, deduped case-insensitively, order preserved).
pub async fn set_tags(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
    AppJson(body): AppJson<TagsBody>,
) -> Result<Json<Book>, AppError> {
    let mut tags: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for raw in body.tags {
        let tag = raw.trim().to_string();
        if tag.is_empty() {
            continue;
        }
        if tag.chars().count() > MAX_TAG_CHARS {
            return Err(AppError::bad_request("tags must be at most 24 characters"));
        }
        if seen.insert(tag.to_lowercase()) {
            tags.push(tag);
        }
    }
    if tags.len() > MAX_TAGS {
        return Err(AppError::bad_request("at most 12 tags per book"));
    }
    let tags_json = serde_json::to_string(&tags).map_err(AppError::internal)?;
    let book = state
        .db
        .call(move |c| {
            if !db::set_tags(c, &user.id, &id, &tags_json)? {
                return Ok(None);
            }
            db::get_book(c, &user.id, &id)
        })
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(book))
}
