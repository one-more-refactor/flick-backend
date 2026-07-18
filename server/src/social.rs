//! Social layer (contract v0.7): friends via personal link (auto-mutual),
//! a scoreboard of AGGREGATE stats only (friends never see titles or
//! content — words, streaks and time are the whole surface), and the yearly
//! wrapped breakdown.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{random_token, AuthUser};
use crate::db::{self, now_secs};
use crate::error::{AppError, AppJson, AppPath};
use crate::stats::{parse_day, streaks, today_epoch_days, utc_day, DAILY_GOAL};
use crate::AppState;

/// GET /api/friends/link — the caller's friend link (same personal code as
/// referrals; possession of the link = consent to connect).
pub async fn link(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>, AppError> {
    if user.guest {
        return Err(AppError::Status(
            StatusCode::FORBIDDEN,
            "create an account first — friends need somewhere to stick".into(),
        ));
    }
    let fresh = random_token(16);
    let uid = user.id.clone();
    let code = state
        .db
        .call(move |c| db::ensure_ref_code(c, &uid, &fresh))
        .await?;
    Ok(Json(json!({"code": code, "path": format!("/f/{code}")})))
}

#[derive(Deserialize)]
pub struct AddBody {
    code: String,
}

/// POST /api/friends/add {code} — connect (auto-mutual). 404 unknown code,
/// 409 when it's your own.
pub async fn add(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppJson(body): AppJson<AddBody>,
) -> Result<StatusCode, AppError> {
    let code = body.code.trim().trim_start_matches("/f/").to_string();
    let now = now_secs();
    let uid = user.id.clone();
    let added = state
        .db
        .call(move |c| {
            let Some(other) = db::user_id_by_ref_code(c, &code)? else {
                return Ok(None);
            };
            if other == uid {
                return Ok(Some(false));
            }
            db::add_friend(c, &uid, &other, now).map(Some)
        })
        .await?;
    match added {
        None => Err(AppError::NotFound),
        Some(false) => Err(AppError::Conflict("that's your own link".into())),
        Some(true) => Ok(StatusCode::NO_CONTENT),
    }
}

/// DELETE /api/friends/{id} — unfriend (either side).
pub async fn remove(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let gone = state
        .db
        .call(move |c| db::remove_friend(c, &user.id, &id))
        .await?;
    if gone {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

/// One scoreboard row: aggregates only, never content.
fn score_row(c: &rusqlite::Connection, id: &str, me: bool) -> rusqlite::Result<Value> {
    let Some(u) = db::user_by_id(c, id)? else {
        return Ok(Value::Null);
    };
    let days = db::reading_days(c, id)?;
    let total: i64 = days.iter().map(|(_, w)| w).sum();
    let today = utc_day(0);
    let today_words = days
        .iter()
        .find(|(d, _)| *d == today)
        .map(|(_, w)| *w)
        .unwrap_or(0);
    // Words in the last 7 calendar days (the scoreboard's race).
    let week_floor = utc_day(-6);
    let week: i64 = days
        .iter()
        .filter(|(d, _)| d.as_str() >= week_floor.as_str())
        .map(|(_, w)| w)
        .sum();
    let qualifying: Vec<i64> = days
        .iter()
        .filter(|(_, w)| *w >= DAILY_GOAL)
        .filter_map(|(d, _)| parse_day(d))
        .collect();
    let (current, best) = streaks(&qualifying, today_epoch_days());
    Ok(json!({
        "id": u.id,
        "name": u.username.or(if u.guest { None } else { Some(u.name) }).unwrap_or_else(|| "reader".into()),
        "me": me,
        "week_words": week,
        "total_words": total,
        "today_words": today_words,
        "streak": current,
        "best_streak": best,
    }))
}

/// GET /api/friends — self + friends as scoreboard rows.
pub async fn list(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>, AppError> {
    let uid = user.id.clone();
    let rows = state
        .db
        .call(move |c| {
            let mut out = vec![score_row(c, &uid, true)?];
            for fid in db::friend_ids(c, &uid)? {
                let row = score_row(c, &fid, false)?;
                if !row.is_null() {
                    out.push(row);
                }
            }
            Ok::<_, rusqlite::Error>(out)
        })
        .await?;
    Ok(Json(Value::Array(rows)))
}

#[derive(Deserialize)]
pub struct WrappedQuery {
    year: Option<i64>,
}

/// GET /api/wrapped?year=YYYY — the yearly breakdown (defaults to the year
/// of the server's current UTC date).
pub async fn wrapped(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<WrappedQuery>,
) -> Result<Json<Value>, AppError> {
    let current_year: i64 = utc_day(0)[..4].parse().unwrap_or(1970);
    let year = q.year.unwrap_or(current_year).clamp(2020, current_year);
    let prefix = format!("{year:04}-");
    let start = parse_day(&format!("{year:04}-01-01")).unwrap_or(0) * 86_400;
    let end = parse_day(&format!("{:04}-01-01", year + 1)).unwrap_or(0) * 86_400;

    let uid = user.id.clone();
    let out = state
        .db
        .call(move |c| {
            let days: Vec<(String, i64)> = db::reading_days(c, &uid)?
                .into_iter()
                .filter(|(d, _)| d.starts_with(&prefix))
                .collect();
            let total_words: i64 = days.iter().map(|(_, w)| w).sum();
            let active_days = days.iter().filter(|(_, w)| *w > 0).count() as i64;
            let best_day = days
                .iter()
                .max_by_key(|(_, w)| *w)
                .map(|(d, w)| json!({"day": d, "words": w}));
            // Longest streak within the year.
            let qualifying: Vec<i64> = days
                .iter()
                .filter(|(_, w)| *w >= DAILY_GOAL)
                .filter_map(|(d, _)| parse_day(d))
                .collect();
            let (_, best_streak) = streaks(&qualifying, 0);
            // Top month + busiest weekday (1970-01-01 was a Thursday → +3).
            let mut months = [0i64; 13];
            let mut weekdays = [0i64; 7];
            for (d, w) in &days {
                if let Some(m) = d[5..7].parse::<usize>().ok().filter(|m| (1..=12).contains(m)) {
                    months[m] += w;
                }
                if let Some(ed) = parse_day(d) {
                    weekdays[((ed + 3).rem_euclid(7)) as usize] += w;
                }
            }
            let top_month = (1..=12).max_by_key(|&m| months[m]).unwrap_or(1);
            let top_weekday = (0..7).max_by_key(|&d| weekdays[d]).unwrap_or(0);
            let (sessions, time_ms, sess_words) = db::sessions_between(c, &uid, start, end)?;
            let avg_wpm = if time_ms > 0 {
                (sess_words as f64 / (time_ms as f64 / 60_000.0)).round() as i64
            } else {
                0
            };
            let books_finished = db::books_finished_between(c, &uid, start, end)?;
            Ok::<_, rusqlite::Error>(json!({
                "year": year,
                "total_words": total_words,
                "active_days": active_days,
                "best_day": best_day,
                "best_streak": best_streak,
                "top_month": if months[top_month] > 0 { Some(top_month) } else { None },
                "top_weekday": if weekdays[top_weekday] > 0 { Some(top_weekday) } else { None },
                "sessions": sessions,
                "time_ms": time_ms,
                "avg_wpm": avg_wpm,
                "books_finished": books_finished,
            }))
        })
        .await?;
    Ok(Json(out))
}
