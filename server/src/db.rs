//! SQLite storage: a single bundled-SQLite connection behind a mutex,
//! accessed via `spawn_blocking` so queries never block the async runtime.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

use crate::error::AppError;

const SCHEMA_V1: &str = "
CREATE TABLE users (
    id            TEXT PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL,
    password_hash TEXT,
    oidc_sub      TEXT UNIQUE,
    created_at    INTEGER NOT NULL
);
CREATE TABLE sessions (
    token      TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE TABLE books (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title      TEXT NOT NULL,
    source     TEXT NOT NULL,
    word_count INTEGER NOT NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    timeline   BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX books_user ON books(user_id, created_at);
";

const SCHEMA_V2: &str = "
ALTER TABLE users ADD COLUMN username TEXT;
ALTER TABLE users ADD COLUMN onboarded INTEGER NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN wpm INTEGER NOT NULL DEFAULT 350;
ALTER TABLE users ADD COLUMN theme TEXT NOT NULL DEFAULT 'auto';
";

/// v0.3: guests (nullable email forces a users-table rebuild), settings
/// accent/lang, book metadata, identities (multi-provider OAuth), reading
/// stats, login codes, and the catalog timeline cache. The legacy
/// `users.oidc_sub` column is kept (SQLite can't drop it cheaply) but its
/// values migrate into `identities` and it is never read again.
const SCHEMA_V3: &str = "
CREATE TABLE users_v3 (
    id            TEXT PRIMARY KEY,
    email         TEXT UNIQUE,
    name          TEXT NOT NULL,
    password_hash TEXT,
    oidc_sub      TEXT UNIQUE,
    created_at    INTEGER NOT NULL,
    username      TEXT,
    onboarded     INTEGER NOT NULL DEFAULT 0,
    wpm           INTEGER NOT NULL DEFAULT 350,
    theme         TEXT NOT NULL DEFAULT 'auto',
    guest         INTEGER NOT NULL DEFAULT 0,
    accent        TEXT NOT NULL DEFAULT 'red',
    lang          TEXT NOT NULL DEFAULT 'auto'
);
INSERT INTO users_v3 (id, email, name, password_hash, oidc_sub, created_at,
                      username, onboarded, wpm, theme)
    SELECT id, email, name, password_hash, oidc_sub, created_at,
           username, onboarded, wpm, theme
    FROM users;
DROP TABLE users;
ALTER TABLE users_v3 RENAME TO users;
ALTER TABLE books ADD COLUMN last_read_at INTEGER;
ALTER TABLE books ADD COLUMN author TEXT;
ALTER TABLE books ADD COLUMN url TEXT;
ALTER TABLE books ADD COLUMN favicon TEXT;
ALTER TABLE books ADD COLUMN excerpt TEXT;
ALTER TABLE books ADD COLUMN category TEXT;
ALTER TABLE books ADD COLUMN catalog_slug TEXT;
CREATE TABLE identities (
    provider TEXT NOT NULL,
    sub      TEXT NOT NULL,
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email    TEXT,
    PRIMARY KEY (provider, sub)
);
INSERT INTO identities (provider, sub, user_id, email)
    SELECT 'oidc', oidc_sub, id, email FROM users WHERE oidc_sub IS NOT NULL;
CREATE TABLE reading_days (
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    day     TEXT NOT NULL,
    words   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, day)
);
CREATE TABLE sessions_log (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    book_id     TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    words       INTEGER NOT NULL,
    avg_wpm     INTEGER NOT NULL
);
CREATE INDEX sessions_log_user ON sessions_log(user_id, started_at);
CREATE TABLE login_codes (
    email      TEXT PRIMARY KEY,
    code_hash  TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE catalog_cache (
    slug       TEXT PRIMARY KEY,
    timeline   BLOB NOT NULL,
    word_count INTEGER NOT NULL
);
";

/// v0.3b: store each book's source plaintext (needed for `/text` and search)
/// and index title/author/text with an external-content FTS5 table kept in
/// sync by triggers. Pre-existing books get `text = NULL` (still readable,
/// just not searchable / no `/text`, per contract). The FTS index is
/// backfilled from current rows before the triggers take over future writes.
const SCHEMA_V4: &str = "
ALTER TABLE books ADD COLUMN text TEXT;
CREATE VIRTUAL TABLE books_fts USING fts5(
    title, author, text,
    content='books', content_rowid='rowid'
);
INSERT INTO books_fts (rowid, title, author, text)
    SELECT rowid, title, author, text FROM books;
CREATE TRIGGER books_fts_ai AFTER INSERT ON books BEGIN
    INSERT INTO books_fts (rowid, title, author, text)
    VALUES (new.rowid, new.title, new.author, new.text);
END;
CREATE TRIGGER books_fts_ad AFTER DELETE ON books BEGIN
    INSERT INTO books_fts (books_fts, rowid, title, author, text)
    VALUES ('delete', old.rowid, old.title, old.author, old.text);
END;
CREATE TRIGGER books_fts_au AFTER UPDATE ON books BEGIN
    INSERT INTO books_fts (books_fts, rowid, title, author, text)
    VALUES ('delete', old.rowid, old.title, old.author, old.text);
    INSERT INTO books_fts (rowid, title, author, text)
    VALUES (new.rowid, new.title, new.author, new.text);
END;
";

/// v0.4: editions & plans — `users.plan` (`'free'` | `'pro'`). No API sets it
/// (manual/admin only until billing exists); it is only read into user JSON
/// and the hosted upload-limit check.
const SCHEMA_V5: &str = "
ALTER TABLE users ADD COLUMN plan TEXT NOT NULL DEFAULT 'free';
";

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (creating the data dir and schema if missing) `<data_dir>/flick.db`.
    pub fn open(data_dir: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        std::fs::create_dir_all(data_dir)?;
        let conn = Connection::open(data_dir.join("flick.db"))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
            conn.pragma_update(None, "user_version", 1)?;
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 2 {
            conn.execute_batch(SCHEMA_V2)?;
            conn.pragma_update(None, "user_version", 2)?;
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 3 {
            // The users-table rebuild needs FK enforcement off (standard
            // SQLite table-rebuild dance); the batch itself is transactional.
            conn.pragma_update(None, "foreign_keys", "OFF")?;
            let migrated = conn.execute_batch(&format!("BEGIN;\n{SCHEMA_V3}\nCOMMIT;"));
            conn.pragma_update(None, "foreign_keys", "ON")?;
            migrated?;
            conn.pragma_update(None, "user_version", 3)?;
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 4 {
            conn.execute_batch(&format!("BEGIN;\n{SCHEMA_V4}\nCOMMIT;"))?;
            conn.pragma_update(None, "user_version", 4)?;
        }
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 5 {
            conn.execute_batch(&format!("BEGIN;\n{SCHEMA_V5}\nCOMMIT;"))?;
            conn.pragma_update(None, "user_version", 5)?;
        }
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure against the connection on a blocking thread.
    pub async fn call<T, F>(&self, f: F) -> Result<T, AppError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            f(&conn)
        })
        .await
        .map_err(|e| AppError::Internal(format!("db task join: {e}")))?
        .map_err(AppError::from)
    }
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- users

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: Option<String>,
    pub name: String,
    pub password_hash: Option<String>,
    pub username: Option<String>,
    pub onboarded: bool,
    pub wpm: i64,
    pub theme: String,
    pub guest: bool,
    pub accent: String,
    pub lang: String,
    pub plan: String,
}

fn row_user(r: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        password_hash: r.get(3)?,
        username: r.get(4)?,
        onboarded: r.get::<_, i64>(5)? != 0,
        wpm: r.get(6)?,
        theme: r.get(7)?,
        guest: r.get::<_, i64>(8)? != 0,
        accent: r.get(9)?,
        lang: r.get(10)?,
        plan: r.get(11)?,
    })
}

const USER_COLS: &str =
    "id, email, name, password_hash, username, onboarded, wpm, theme, guest, accent, lang, plan";
const USER_COLS_U: &str = "u.id, u.email, u.name, u.password_hash, u.username, u.onboarded, \
                           u.wpm, u.theme, u.guest, u.accent, u.lang, u.plan";

pub fn user_by_email(c: &Connection, email: &str) -> rusqlite::Result<Option<User>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE email = ?1"),
        [email],
        row_user,
    )
    .optional()
}

pub fn insert_user(c: &Connection, user: &User, now: i64) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO users (id, email, name, password_hash, created_at,
                            username, onboarded, wpm, theme, guest, accent, lang, plan)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            user.id,
            user.email,
            user.name,
            user.password_hash,
            now,
            user.username,
            user.onboarded as i64,
            user.wpm,
            user.theme,
            user.guest as i64,
            user.accent,
            user.lang,
            user.plan
        ],
    )?;
    Ok(())
}

/// Persist the mutable profile fields of an already-merged `User`.
pub fn update_profile(c: &Connection, user: &User) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE users SET name = ?2, username = ?3, onboarded = ?4, wpm = ?5, theme = ?6,
                          accent = ?7, lang = ?8
         WHERE id = ?1",
        params![
            user.id,
            user.name,
            user.username,
            user.onboarded as i64,
            user.wpm,
            user.theme,
            user.accent,
            user.lang
        ],
    )?;
    Ok(())
}

// ------------------------------------------------------------ identities

pub fn user_by_identity(
    c: &Connection,
    provider: &str,
    sub: &str,
) -> rusqlite::Result<Option<User>> {
    c.query_row(
        &format!(
            "SELECT {USER_COLS_U} FROM identities i JOIN users u ON u.id = i.user_id
             WHERE i.provider = ?1 AND i.sub = ?2"
        ),
        params![provider, sub],
        row_user,
    )
    .optional()
}

pub fn link_identity(
    c: &Connection,
    provider: &str,
    sub: &str,
    user_id: &str,
    email: Option<&str>,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO identities (provider, sub, user_id, email)
         VALUES (?1, ?2, ?3, ?4)",
        params![provider, sub, user_id, email],
    )?;
    Ok(())
}

/// Distinct OAuth providers linked to a user (for the lookup `methods` list).
pub fn identity_providers(c: &Connection, user_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = c.prepare(
        "SELECT DISTINCT provider FROM identities WHERE user_id = ?1 ORDER BY provider",
    )?;
    let rows = stmt.query_map([user_id], |r| r.get(0))?;
    rows.collect()
}

// ------------------------------------------------------------ guest merge

/// Merge a guest account into `target_id`: books, reading days (summing on
/// day collision) and the session log move over, then the guest row is
/// deleted (cascading its auth sessions). The intro book never duplicates —
/// when the target already has one, the guest's copy is dropped. No-op when
/// `guest_id` is not actually a guest.
pub fn merge_guest_into(
    c: &Connection,
    guest_id: &str,
    target_id: &str,
) -> rusqlite::Result<()> {
    let is_guest: Option<i64> = c
        .query_row("SELECT guest FROM users WHERE id = ?1", [guest_id], |r| {
            r.get(0)
        })
        .optional()?;
    if is_guest != Some(1) || guest_id == target_id {
        return Ok(());
    }
    let tx = c.unchecked_transaction()?;
    let target_intros: i64 = tx.query_row(
        "SELECT COUNT(*) FROM books WHERE user_id = ?1 AND source = 'intro'",
        [target_id],
        |r| r.get(0),
    )?;
    if target_intros > 0 {
        tx.execute(
            "DELETE FROM books WHERE user_id = ?1 AND source = 'intro'",
            [guest_id],
        )?;
    }
    // Both sides carry the seeded catalog (contract "Starter library"): for
    // each slug present on both, keep the copy that was read further — drop
    // the target's copy when the guest got past it, then any guest copy whose
    // slug still exists on the target.
    tx.execute(
        "DELETE FROM books WHERE user_id = ?2 AND catalog_slug IS NOT NULL AND EXISTS (
             SELECT 1 FROM books g
             WHERE g.user_id = ?1 AND g.catalog_slug = books.catalog_slug
               AND g.position > books.position
         )",
        params![guest_id, target_id],
    )?;
    tx.execute(
        "DELETE FROM books WHERE user_id = ?1 AND catalog_slug IS NOT NULL AND EXISTS (
             SELECT 1 FROM books t
             WHERE t.user_id = ?2 AND t.catalog_slug = books.catalog_slug
         )",
        params![guest_id, target_id],
    )?;
    tx.execute(
        "UPDATE books SET user_id = ?2 WHERE user_id = ?1",
        params![guest_id, target_id],
    )?;
    tx.execute(
        "INSERT INTO reading_days (user_id, day, words)
             SELECT ?2, day, words FROM reading_days WHERE user_id = ?1
         ON CONFLICT(user_id, day) DO UPDATE SET words = words + excluded.words",
        params![guest_id, target_id],
    )?;
    tx.execute("DELETE FROM reading_days WHERE user_id = ?1", [guest_id])?;
    tx.execute(
        "UPDATE sessions_log SET user_id = ?2 WHERE user_id = ?1",
        params![guest_id, target_id],
    )?;
    tx.execute("DELETE FROM users WHERE id = ?1", [guest_id])?;
    tx.commit()
}

// ------------------------------------------------------------- sessions

pub fn create_session(
    c: &Connection,
    token: &str,
    user_id: &str,
    now: i64,
    expires_at: i64,
) -> rusqlite::Result<()> {
    // Opportunistically drop expired sessions.
    c.execute("DELETE FROM sessions WHERE expires_at < ?1", [now])?;
    c.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![token, user_id, now, expires_at],
    )?;
    Ok(())
}

pub fn session_user(c: &Connection, token: &str, now: i64) -> rusqlite::Result<Option<User>> {
    c.query_row(
        &format!(
            "SELECT {USER_COLS_U}
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token = ?1 AND s.expires_at >= ?2"
        ),
        params![token, now],
        row_user,
    )
    .optional()
}

pub fn delete_session(c: &Connection, token: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM sessions WHERE token = ?1", [token])?;
    Ok(())
}

// ----------------------------------------------------------- login codes

pub fn upsert_login_code(
    c: &Connection,
    email: &str,
    code_hash: &str,
    expires_at: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO login_codes (email, code_hash, expires_at, attempts)
         VALUES (?1, ?2, ?3, 0)",
        params![email, code_hash, expires_at],
    )?;
    Ok(())
}

/// `(code_hash, expires_at, attempts)` for a pending code.
pub fn login_code(
    c: &Connection,
    email: &str,
) -> rusqlite::Result<Option<(String, i64, i64)>> {
    c.query_row(
        "SELECT code_hash, expires_at, attempts FROM login_codes WHERE email = ?1",
        [email],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .optional()
}

pub fn bump_login_code_attempts(c: &Connection, email: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE login_codes SET attempts = attempts + 1 WHERE email = ?1",
        [email],
    )?;
    Ok(())
}

pub fn delete_login_code(c: &Connection, email: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM login_codes WHERE email = ?1", [email])?;
    Ok(())
}

// ---------------------------------------------------------------- books

#[derive(Debug, Clone, Serialize)]
pub struct Book {
    pub id: String,
    pub title: String,
    pub source: String,
    pub word_count: i64,
    pub position: i64,
    pub created_at: i64,
    pub last_read_at: Option<i64>,
    pub author: Option<String>,
    pub url: Option<String>,
    pub favicon: Option<String>,
    pub excerpt: Option<String>,
    pub category: Option<String>,
}

fn row_book(r: &Row) -> rusqlite::Result<Book> {
    Ok(Book {
        id: r.get(0)?,
        title: r.get(1)?,
        source: r.get(2)?,
        word_count: r.get(3)?,
        position: r.get(4)?,
        created_at: r.get(5)?,
        last_read_at: r.get(6)?,
        author: r.get(7)?,
        url: r.get(8)?,
        favicon: r.get(9)?,
        excerpt: r.get(10)?,
        category: r.get(11)?,
    })
}

const BOOK_COLS: &str = "id, title, source, word_count, position, created_at, \
                         last_read_at, author, url, favicon, excerpt, category";

pub fn insert_book(
    c: &Connection,
    user_id: &str,
    book: &Book,
    timeline: &[u8],
    text: Option<&str>,
    catalog_slug: Option<&str>,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO books (id, user_id, title, source, word_count, position, timeline,
                            created_at, last_read_at, author, url, favicon, excerpt,
                            category, catalog_slug, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            book.id,
            user_id,
            book.title,
            book.source,
            book.word_count,
            book.position,
            timeline,
            book.created_at,
            book.last_read_at,
            book.author,
            book.url,
            book.favicon,
            book.excerpt,
            book.category,
            catalog_slug,
            text
        ],
    )?;
    Ok(())
}

/// The stored source plaintext for a user's book, or `None` when the book is
/// missing or predates text storage (v0.3b backfilled such rows as NULL).
pub fn book_text(c: &Connection, user_id: &str, id: &str) -> rusqlite::Result<Option<String>> {
    c.query_row(
        "SELECT text FROM books WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
}

/// Full-text search a user's books (title + author + text) via FTS5, ordered
/// by relevance. `match_query` must already be a safe FTS5 query string.
pub fn search_books(
    c: &Connection,
    user_id: &str,
    match_query: &str,
) -> rusqlite::Result<Vec<Book>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {} FROM books_fts
         JOIN books b ON b.rowid = books_fts.rowid
         WHERE books_fts MATCH ?1 AND b.user_id = ?2
         ORDER BY books_fts.rank",
        BOOK_COLS
            .split(", ")
            .map(|c| format!("b.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    let rows = stmt.query_map(params![match_query, user_id], row_book)?;
    rows.collect()
}

pub fn list_books(c: &Connection, user_id: &str) -> rusqlite::Result<Vec<Book>> {
    let mut stmt = c.prepare(&format!(
        "SELECT {BOOK_COLS} FROM books WHERE user_id = ?1 ORDER BY created_at DESC, id"
    ))?;
    let rows = stmt.query_map([user_id], row_book)?;
    rows.collect()
}

pub fn get_book(c: &Connection, user_id: &str, id: &str) -> rusqlite::Result<Option<Book>> {
    c.query_row(
        &format!("SELECT {BOOK_COLS} FROM books WHERE id = ?1 AND user_id = ?2"),
        params![id, user_id],
        row_book,
    )
    .optional()
}

pub fn book_count(c: &Connection, user_id: &str) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM books WHERE user_id = ?1",
        [user_id],
        |r| r.get(0),
    )
}

/// 00:00:00 UTC of the Monday opening `now`'s ISO-8601 week. Pure epoch
/// arithmetic (1970-01-01 was a Thursday — ISO weekday 3, Monday = 0), so no
/// date crate is needed; ISO weeks in UTC are exactly Monday-aligned 7-day
/// spans of epoch days.
pub fn iso_week_start(now: i64) -> i64 {
    let days = now.div_euclid(86_400);
    let weekday = (days + 3).rem_euclid(7); // Monday = 0
    (days - weekday) * 86_400
}

/// User-sourced ingestions this ISO week (UTC): every book insert EXCEPT the
/// intro seed and catalog copies (CONTRACTS.md "Editions & plans"). Derived
/// from `books.created_at` instead of a separate counter table — deleting a
/// book therefore refunds its upload for the week, a deliberate (and
/// friendly) trade-off in exchange for having no second source of truth.
pub fn uploads_this_week(c: &Connection, user_id: &str, now: i64) -> rusqlite::Result<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM books
         WHERE user_id = ?1 AND created_at >= ?2
           AND source NOT IN ('intro', 'catalog')",
        params![user_id, iso_week_start(now)],
        |r| r.get(0),
    )
}

/// Existing library entry for a catalog work (add is idempotent per user).
pub fn book_id_by_catalog_slug(
    c: &Connection,
    user_id: &str,
    slug: &str,
) -> rusqlite::Result<Option<String>> {
    c.query_row(
        "SELECT id FROM books WHERE user_id = ?1 AND catalog_slug = ?2",
        params![user_id, slug],
        |r| r.get(0),
    )
    .optional()
}

pub fn get_timeline(c: &Connection, user_id: &str, id: &str) -> rusqlite::Result<Option<Vec<u8>>> {
    c.query_row(
        "SELECT timeline FROM books WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
        |r| r.get(0),
    )
    .optional()
}

pub fn set_position(
    c: &Connection,
    user_id: &str,
    id: &str,
    position: i64,
    now: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE books SET position = ?3, last_read_at = ?4 WHERE id = ?1 AND user_id = ?2",
        params![id, user_id, position, now],
    )?;
    Ok(())
}

/// Returns false when no such book belongs to the user.
pub fn delete_book(c: &Connection, user_id: &str, id: &str) -> rusqlite::Result<bool> {
    let n = c.execute(
        "DELETE FROM books WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )?;
    Ok(n > 0)
}

// ---------------------------------------------------------- reading days

/// Add consumed words to the user's row for `day` (creating it as needed).
pub fn add_read_words(
    c: &Connection,
    user_id: &str,
    day: &str,
    words: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO reading_days (user_id, day, words) VALUES (?1, ?2, ?3)
         ON CONFLICT(user_id, day) DO UPDATE SET words = words + excluded.words",
        params![user_id, day, words],
    )?;
    Ok(())
}

/// All `(day, words)` rows for a user, oldest first.
pub fn reading_days(c: &Connection, user_id: &str) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt =
        c.prepare("SELECT day, words FROM reading_days WHERE user_id = ?1 ORDER BY day")?;
    let rows = stmt.query_map([user_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    rows.collect()
}

// ----------------------------------------------------------- session log

#[derive(Debug, Clone, Serialize)]
pub struct SessionLog {
    pub id: String,
    pub book_id: String,
    pub started_at: i64,
    pub duration_ms: i64,
    pub words: i64,
    pub avg_wpm: i64,
}

/// Lifetime aggregates for GET /api/stats "totals" (contract v0.4.2).
pub struct StatsTotals {
    pub time_ms: i64,
    pub sessions: i64,
    pub session_words: i64,
    pub books_finished: i64,
    pub active_days: i64,
    pub best_day: Option<(String, i64)>,
}

pub fn stats_totals(c: &Connection, user_id: &str) -> rusqlite::Result<StatsTotals> {
    let (time_ms, sessions, session_words) = c.query_row(
        "SELECT COALESCE(SUM(duration_ms), 0), COUNT(*), COALESCE(SUM(words), 0)
         FROM sessions_log WHERE user_id = ?1",
        [user_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let books_finished = c.query_row(
        "SELECT COUNT(*) FROM books
         WHERE user_id = ?1 AND word_count > 0 AND position >= word_count",
        [user_id],
        |r| r.get(0),
    )?;
    let active_days = c.query_row(
        "SELECT COUNT(*) FROM reading_days WHERE user_id = ?1 AND words > 0",
        [user_id],
        |r| r.get(0),
    )?;
    let best_day = c
        .query_row(
            "SELECT day, words FROM reading_days
             WHERE user_id = ?1 AND words > 0
             ORDER BY words DESC, day DESC LIMIT 1",
            [user_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()?;
    Ok(StatsTotals {
        time_ms,
        sessions,
        session_words,
        books_finished,
        active_days,
        best_day,
    })
}

pub fn insert_session_log(
    c: &Connection,
    user_id: &str,
    s: &SessionLog,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO sessions_log (id, user_id, book_id, started_at, duration_ms, words, avg_wpm)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            s.id, user_id, s.book_id, s.started_at, s.duration_ms, s.words, s.avg_wpm
        ],
    )?;
    Ok(())
}

/// Newest-first session summaries with the book title when it still exists
/// (the join is user-scoped so a recycled id can never leak a foreign title).
pub fn list_sessions_log(
    c: &Connection,
    user_id: &str,
    limit: i64,
) -> rusqlite::Result<Vec<(SessionLog, Option<String>)>> {
    let mut stmt = c.prepare(
        "SELECT s.id, s.book_id, s.started_at, s.duration_ms, s.words, s.avg_wpm, b.title
         FROM sessions_log s
         LEFT JOIN books b ON b.id = s.book_id AND b.user_id = s.user_id
         WHERE s.user_id = ?1
         ORDER BY s.started_at DESC, s.id
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![user_id, limit], |r| {
        Ok((
            SessionLog {
                id: r.get(0)?,
                book_id: r.get(1)?,
                started_at: r.get(2)?,
                duration_ms: r.get(3)?,
                words: r.get(4)?,
                avg_wpm: r.get(5)?,
            },
            r.get(6)?,
        ))
    })?;
    rows.collect()
}

// ---------------------------------------------------------- catalog cache

/// Cached parse of a catalog work, `(timeline_json, word_count)`.
pub fn catalog_cache_get(
    c: &Connection,
    slug: &str,
) -> rusqlite::Result<Option<(Vec<u8>, i64)>> {
    c.query_row(
        "SELECT timeline, word_count FROM catalog_cache WHERE slug = ?1",
        [slug],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

pub fn catalog_cache_word_count(c: &Connection, slug: &str) -> rusqlite::Result<Option<i64>> {
    c.query_row(
        "SELECT word_count FROM catalog_cache WHERE slug = ?1",
        [slug],
        |r| r.get(0),
    )
    .optional()
}

pub fn catalog_cache_put(
    c: &Connection,
    slug: &str,
    timeline: &[u8],
    word_count: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT OR REPLACE INTO catalog_cache (slug, timeline, word_count) VALUES (?1, ?2, ?3)",
        params![slug, timeline, word_count],
    )?;
    Ok(())
}
