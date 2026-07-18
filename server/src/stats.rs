//! Reading stats: the per-day word log behind streaks, and running-app-style
//! session summaries. Day math is pure UTC civil-date arithmetic (no chrono);
//! the *client* reports its local date because streaks are a human-day
//! concept (CONTRACTS.md).

use std::collections::HashSet;

use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{random_token, AuthUser};
use crate::db::{self, now_secs, SessionLog};
use crate::error::{AppError, AppJson};
use crate::AppState;

/// Daily words threshold for a streak day (server constant per contract).
pub const DAILY_GOAL: i64 = 300;
/// Per-report cap on `read` in position saves.
pub const MAX_READ_PER_REPORT: i64 = 500;
/// A client-reported `day` may differ from the server date by at most this.
pub const MAX_DAY_SKEW_DAYS: i64 = 2;

const MAX_SESSION_MS: i64 = 6 * 60 * 60 * 1000; // 6h
const MAX_AVG_WPM: i64 = 1500;
const DEFAULT_SESSIONS_LIMIT: i64 = 50;
const MAX_SESSIONS_LIMIT: i64 = 200;
/// Free hosted accounts keep this much session history (contract: Pro =
/// unlimited reading history; enforced server-side so clients can't cheat).
pub const FREE_HISTORY_DAYS: i64 = 90;

// ------------------------------------------------------------ day math
// Howard Hinnant's civil-date algorithms (proleptic Gregorian).

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days since the Unix epoch, UTC, for the current server time.
pub(crate) fn today_epoch_days() -> i64 {
    now_secs().div_euclid(86400)
}

/// The server's UTC date shifted by `offset_days`, as `YYYY-MM-DD`.
/// Public so tests can compute "yesterday" the same way the server does.
pub fn utc_day(offset_days: i64) -> String {
    let (y, m, d) = civil_from_days(today_epoch_days() + offset_days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Strict `YYYY-MM-DD` → days since the Unix epoch. Rejects malformed
/// strings and impossible dates (round-trip check catches Feb 30 etc.).
pub(crate) fn parse_day(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        if !s[r.clone()].bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s[r].parse().ok()
    };
    let (y, m, d) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    (civil_from_days(days) == (y, m, d)).then_some(days)
}

// -------------------------------------------------------------- streaks

/// `(current, best)` from the ascending qualifying days (epoch days).
/// Current = consecutive run ending today or yesterday.
fn streaks(qualifying_asc: &[i64], today: i64) -> (i64, i64) {
    let mut best = 0i64;
    let mut run = 0i64;
    let mut prev: Option<i64> = None;
    for &day in qualifying_asc {
        run = if prev == Some(day - 1) { run + 1 } else { 1 };
        best = best.max(run);
        prev = Some(day);
    }
    let set: HashSet<i64> = qualifying_asc.iter().copied().collect();
    let mut anchor = if set.contains(&today) {
        today
    } else if set.contains(&(today - 1)) {
        today - 1
    } else {
        return (0, best);
    };
    let mut current = 0i64;
    while set.contains(&anchor) {
        current += 1;
        anchor -= 1;
    }
    (current, best)
}

// -------------------------------------------------------------- handlers

/// GET /api/stats
pub async fn stats(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>, AppError> {
    let (days, totals) = state
        .db
        .call(move |c| {
            Ok::<_, rusqlite::Error>((
                db::reading_days(c, &user.id)?,
                db::stats_totals(c, &user.id)?,
            ))
        })
        .await?;

    let today_str = utc_day(0);
    let total_words: i64 = days.iter().map(|(_, w)| w).sum();
    let today_words = days
        .iter()
        .find(|(d, _)| *d == today_str)
        .map(|(_, w)| *w)
        .unwrap_or(0);
    let qualifying: Vec<i64> = days
        .iter()
        .filter(|(_, w)| *w >= DAILY_GOAL)
        .filter_map(|(d, _)| parse_day(d))
        .collect();
    let (current, best) = streaks(&qualifying, today_epoch_days());
    let last: Vec<Value> = days
        .iter()
        .rev()
        .take(42)
        .rev()
        .map(|(day, words)| json!({"day": day, "words": words}))
        .collect();

    // Duration-weighted lifetime average wpm from the session log.
    let avg_wpm = if totals.time_ms > 0 {
        (totals.session_words as f64 / (totals.time_ms as f64 / 60_000.0)).round() as i64
    } else {
        0
    };

    Ok(Json(json!({
        "today": {"day": today_str, "words": today_words},
        "total_words": total_words,
        "goal": DAILY_GOAL,
        "streak": {"current": current, "best": best},
        "days": last,
        "totals": {
            "time_ms": totals.time_ms,
            "sessions": totals.sessions,
            "avg_wpm": avg_wpm,
            "books_finished": totals.books_finished,
            "active_days": totals.active_days,
            "best_day": totals.best_day
                .map(|(day, words)| json!({"day": day, "words": words})),
        },
    })))
}

#[derive(Deserialize)]
pub struct SessionBody {
    book_id: String,
    started_at: i64,
    duration_ms: i64,
    words: i64,
    avg_wpm: i64,
}

/// POST /api/sessions — store a session summary with sanity clamps
/// (duration ≤ 6h, avg_wpm ≤ 1500, words consistent with duration×wpm ±50%).
pub async fn create_session(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppJson(body): AppJson<SessionBody>,
) -> Result<Response, AppError> {
    if body.started_at < 0 || body.duration_ms <= 0 || body.words < 0 || body.avg_wpm < 0 {
        return Err(AppError::bad_request(
            "started_at, duration_ms, words and avg_wpm must be non-negative",
        ));
    }
    let duration_ms = body.duration_ms.min(MAX_SESSION_MS);
    let avg_wpm = body.avg_wpm.min(MAX_AVG_WPM);
    let expected = duration_ms as f64 / 60_000.0 * avg_wpm as f64;
    let words = (body.words as f64)
        .clamp(expected * 0.5, expected * 1.5)
        .round() as i64;

    let session = SessionLog {
        id: random_token(16),
        book_id: body.book_id,
        started_at: body.started_at,
        duration_ms,
        words,
        avg_wpm,
    };
    let stored = session.clone();
    state
        .db
        .call(move |c| db::insert_session_log(c, &user.id, &stored))
        .await?;
    Ok((StatusCode::CREATED, Json(session)).into_response())
}

#[derive(Deserialize)]
struct SessionsQuery {
    limit: Option<i64>,
}

/// GET /api/sessions?limit=50 — newest first, with book titles
/// (`"DELETED"` when the book is gone).
pub async fn list_sessions(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    uri: Uri,
) -> Result<Json<Value>, AppError> {
    let limit = Query::<SessionsQuery>::try_from_uri(&uri)
        .map_err(|e| AppError::bad_request(e.body_text()))?
        .0
        .limit
        .unwrap_or(DEFAULT_SESSIONS_LIMIT)
        .clamp(1, MAX_SESSIONS_LIMIT);
    // Reading-history window: hosted free plan sees the last 90 days only
    // (Pro and selfhost: everything). Server-side by design.
    let min_started_at = if crate::books::weekly_upload_limit(&state.config, &user).is_some() {
        now_secs() - FREE_HISTORY_DAYS * 86_400
    } else {
        0
    };
    let rows = state
        .db
        .call(move |c| db::list_sessions_log(c, &user.id, limit, min_started_at))
        .await?;
    let sessions: Vec<Value> = rows
        .into_iter()
        .map(|(s, title)| {
            json!({
                "id": s.id,
                "book_id": s.book_id,
                "book_title": title.unwrap_or_else(|| "DELETED".into()),
                "started_at": s.started_at,
                "duration_ms": s.duration_ms,
                "words": s.words,
                "avg_wpm": s.avg_wpm,
            })
        })
        .collect();
    Ok(Json(Value::Array(sessions)))
}
