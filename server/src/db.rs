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
    pub email: String,
    pub name: String,
    pub password_hash: Option<String>,
}

fn row_user(r: &Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        password_hash: r.get(3)?,
    })
}

const USER_COLS: &str = "id, email, name, password_hash";

pub fn user_by_email(c: &Connection, email: &str) -> rusqlite::Result<Option<User>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE email = ?1"),
        [email],
        row_user,
    )
    .optional()
}

pub fn user_by_oidc_sub(c: &Connection, sub: &str) -> rusqlite::Result<Option<User>> {
    c.query_row(
        &format!("SELECT {USER_COLS} FROM users WHERE oidc_sub = ?1"),
        [sub],
        row_user,
    )
    .optional()
}

pub fn insert_user(
    c: &Connection,
    user: &User,
    oidc_sub: Option<&str>,
    now: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO users (id, email, name, password_hash, oidc_sub, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            user.id,
            user.email,
            user.name,
            user.password_hash,
            oidc_sub,
            now
        ],
    )?;
    Ok(())
}

pub fn link_oidc_sub(c: &Connection, user_id: &str, sub: &str) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE users SET oidc_sub = ?2 WHERE id = ?1",
        params![user_id, sub],
    )?;
    Ok(())
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
        "SELECT u.id, u.email, u.name, u.password_hash
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token = ?1 AND s.expires_at >= ?2",
        params![token, now],
        row_user,
    )
    .optional()
}

pub fn delete_session(c: &Connection, token: &str) -> rusqlite::Result<()> {
    c.execute("DELETE FROM sessions WHERE token = ?1", [token])?;
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
}

fn row_book(r: &Row) -> rusqlite::Result<Book> {
    Ok(Book {
        id: r.get(0)?,
        title: r.get(1)?,
        source: r.get(2)?,
        word_count: r.get(3)?,
        position: r.get(4)?,
        created_at: r.get(5)?,
    })
}

const BOOK_COLS: &str = "id, title, source, word_count, position, created_at";

pub fn insert_book(
    c: &Connection,
    user_id: &str,
    book: &Book,
    timeline: &[u8],
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO books (id, user_id, title, source, word_count, position, timeline, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            book.id,
            user_id,
            book.title,
            book.source,
            book.word_count,
            book.position,
            timeline,
            book.created_at
        ],
    )?;
    Ok(())
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
) -> rusqlite::Result<()> {
    c.execute(
        "UPDATE books SET position = ?3 WHERE id = ?1 AND user_id = ?2",
        params![id, user_id, position],
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
