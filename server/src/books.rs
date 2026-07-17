//! Books: paste/PDF ingestion via flick-core, timelines, reading position.

use axum::extract::multipart::MultipartError;
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use flick_core::Timeline;
use serde::Deserialize;

use crate::auth::{random_token, AuthUser};
use crate::db::{self, now_secs, Book};
use crate::error::{AppError, AppJson, AppPath};
use crate::AppState;

/// Upload/body cap (CONTRACTS.md: 25 MB).
pub const UPLOAD_LIMIT: usize = 25 * 1024 * 1024;

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
    };
    db::insert_book(c, user_id, &book, &timeline_json, None)
}

/// Guests are not seeded at creation — they get the intro book alongside
/// their FIRST own add instead, so an empty guest library never exists
/// (contract). Call before inserting the user's new book.
pub fn maybe_seed_guest_intro(
    c: &rusqlite::Connection,
    user: &db::User,
    now: i64,
) -> rusqlite::Result<()> {
    if user.guest && db::book_count(c, &user.id)? == 0 {
        seed_intro_book(c, &user.id, now)?;
    }
    Ok(())
}

// --------------------------------------------------------------- handlers

pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Vec<Book>>, AppError> {
    let books = state
        .db
        .call(move |c| db::list_books(c, &user.id))
        .await?;
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

/// POST /api/books — JSON `{title?, text}` or multipart PDF (`file`, `title?`).
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

    let (title, text, source) = if content_type.starts_with("multipart/form-data") {
        let (title, text) = from_pdf_upload(&state, req).await?;
        (title, text, "pdf")
    } else {
        let AppJson(body) = AppJson::<PasteBody>::from_request(req, &state).await?;
        let text = body.text;
        if text.trim().is_empty() {
            return Err(AppError::bad_request("text must not be empty"));
        }
        let title = clean_title(body.title).unwrap_or_else(|| default_title(&text));
        (title, text, "paste")
    };

    // Timeline construction can chew through megabytes of text; keep it off
    // the async runtime. flick-core is the only parser.
    let (timeline_json, word_count) = tokio::task::spawn_blocking(move || {
        let timeline = Timeline::from_text(&text);
        serde_json::to_vec(&timeline).map(|json| (json, timeline.word_count))
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
        author: None,
        url: None,
        favicon: None,
        excerpt: None,
        category: None,
    };
    let stored = book.clone();
    state
        .db
        .call(move |c| {
            maybe_seed_guest_intro(c, &user, stored.created_at)?;
            db::insert_book(c, &user.id, &stored, &timeline_json, None)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(book)).into_response())
}

/// Pull `title` + `file` out of the multipart body and extract PDF text.
async fn from_pdf_upload(
    state: &AppState,
    req: Request,
) -> Result<(String, String), AppError> {
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
    if !file.starts_with(b"%PDF") {
        return Err(AppError::bad_request("only PDF uploads are supported"));
    }

    // pdf-extract is known to panic on malformed PDFs: catch_unwind + map
    // anything that goes wrong to a 400.
    let extracted = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&file))
    })
    .await
    .map_err(AppError::internal)?;
    let text = match extracted {
        Ok(Ok(text)) => text,
        _ => {
            return Err(AppError::bad_request(
                "could not extract text from this PDF",
            ))
        }
    };
    if text.trim().is_empty() {
        return Err(AppError::bad_request(
            "could not extract text from this PDF",
        ));
    }

    let title = clean_title(title)
        .or_else(|| {
            filename
                .as_deref()
                .map(|f| f.trim_end_matches(".pdf").trim().to_string())
                .filter(|f| !f.is_empty())
        })
        .unwrap_or_else(|| default_title(&text));
    Ok((title, text))
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

pub async fn delete_book(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let deleted = state
        .db
        .call(move |c| db::delete_book(c, &user.id, &id))
        .await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}
