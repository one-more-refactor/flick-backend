//! Built-in public-domain catalog (CONTRACTS.md): works are embedded at
//! compile time from `server/assets/catalog/`, parsed lazily once per slug
//! into the `catalog_cache` table, then copied cheaply into user libraries.

use std::sync::LazyLock;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use flick_core::Timeline;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{random_token, AuthUser};
use crate::db::{self, now_secs, Book};
use crate::error::{AppError, AppPath};
use crate::AppState;

#[derive(Debug, Clone, Deserialize)]
struct CatalogEntry {
    slug: String,
    file: String,
    title: String,
    author: String,
    lang: String,
    kind: String,
    description: String,
}

static MANIFEST: LazyLock<Vec<CatalogEntry>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../assets/catalog/catalog.json"))
        .expect("valid embedded catalog.json")
});

/// The raw texts, matched to the manifest via its `file` field.
static TEXTS: &[(&str, &str)] = &[
    (
        "gift-of-the-magi.txt",
        include_str!("../assets/catalog/gift-of-the-magi.txt"),
    ),
    (
        "tell-tale-heart.txt",
        include_str!("../assets/catalog/tell-tale-heart.txt"),
    ),
    (
        "owl-creek-bridge.txt",
        include_str!("../assets/catalog/owl-creek-bridge.txt"),
    ),
    (
        "aesops-fables.txt",
        include_str!("../assets/catalog/aesops-fables.txt"),
    ),
    (
        "scandal-in-bohemia.txt",
        include_str!("../assets/catalog/scandal-in-bohemia.txt"),
    ),
    (
        "civil-disobedience.txt",
        include_str!("../assets/catalog/civil-disobedience.txt"),
    ),
    (
        "meditations.txt",
        include_str!("../assets/catalog/meditations.txt"),
    ),
    (
        "die-verwandlung.txt",
        include_str!("../assets/catalog/die-verwandlung.txt"),
    ),
    (
        "jekyll-and-hyde.txt",
        include_str!("../assets/catalog/jekyll-and-hyde.txt"),
    ),
];

fn text_for(file: &str) -> &'static str {
    TEXTS
        .iter()
        .find(|(name, _)| *name == file)
        .map(|(_, text)| *text)
        // Manifest and TEXTS are both compiled in; a mismatch is a build bug.
        .expect("catalog manifest entry has an embedded text")
}

/// Contract mapping: catalog `kind` → book `category`.
fn category_for(kind: &str) -> Option<&'static str> {
    match kind {
        "story" | "fables" => Some("story"),
        "essay" => Some("essay"),
        "novella" => Some("book"),
        _ => None,
    }
}

/// Parse-and-cache a work's timeline if it isn't cached yet; returns its
/// word count.
fn ensure_cached(c: &Connection, entry: &CatalogEntry) -> rusqlite::Result<i64> {
    if let Some(word_count) = db::catalog_cache_word_count(c, &entry.slug)? {
        return Ok(word_count);
    }
    let timeline = Timeline::from_text(text_for(&entry.file));
    let timeline_json = serde_json::to_vec(&timeline)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let word_count = timeline.word_count as i64;
    db::catalog_cache_put(c, &entry.slug, &timeline_json, word_count)?;
    Ok(word_count)
}

/// The cached `(timeline_json, word_count)` for a work, parsing on miss.
fn cached_timeline(c: &Connection, entry: &CatalogEntry) -> rusqlite::Result<(Vec<u8>, i64)> {
    ensure_cached(c, entry)?;
    db::catalog_cache_get(c, &entry.slug)
        .map(|hit| hit.expect("catalog_cache row just ensured"))
}

// -------------------------------------------------------------- handlers

/// GET /api/catalog — public, no auth (contract).
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let entries = state
        .db
        .call(|c| {
            MANIFEST
                .iter()
                .map(|entry| {
                    let word_count = ensure_cached(c, entry)?;
                    Ok(json!({
                        "slug": entry.slug,
                        "title": entry.title,
                        "author": entry.author,
                        "lang": entry.lang,
                        "kind": entry.kind,
                        "description": entry.description,
                        "word_count": word_count,
                    }))
                })
                .collect::<rusqlite::Result<Vec<Value>>>()
        })
        .await?;
    Ok(Json(Value::Array(entries)))
}

/// POST /api/catalog/{slug}/add — copy the cached timeline into the user's
/// library. Idempotent per user: a second add is a 409 carrying the
/// existing book id.
pub async fn add(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(slug): AppPath<String>,
) -> Result<Response, AppError> {
    let entry = MANIFEST
        .iter()
        .find(|e| e.slug == slug)
        .cloned()
        .ok_or(AppError::NotFound)?;

    enum Outcome {
        Duplicate(String),
        Created(Box<Book>),
    }
    let now = now_secs();
    let outcome = state
        .db
        .call(move |c| {
            if let Some(book_id) = db::book_id_by_catalog_slug(c, &user.id, &entry.slug)? {
                return Ok(Outcome::Duplicate(book_id));
            }
            let (timeline_json, word_count) = cached_timeline(c, &entry)?;
            crate::books::maybe_seed_guest_intro(c, &user, now)?;
            let book = Book {
                id: random_token(16),
                title: entry.title.clone(),
                source: "catalog".into(),
                word_count,
                position: 0,
                created_at: now,
                last_read_at: None,
                author: Some(entry.author.clone()),
                url: None,
                favicon: None,
                excerpt: None,
                category: category_for(&entry.kind).map(str::to_string),
            };
            db::insert_book(c, &user.id, &book, &timeline_json, Some(&entry.slug))?;
            Ok(Outcome::Created(Box::new(book)))
        })
        .await?;

    match outcome {
        Outcome::Duplicate(book_id) => Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "this catalog book is already in your library",
                "book_id": book_id,
            })),
        )
            .into_response()),
        Outcome::Created(book) => Ok((StatusCode::CREATED, Json(book)).into_response()),
    }
}
