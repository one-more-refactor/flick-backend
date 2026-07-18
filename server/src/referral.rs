//! Referrals + admin-run global events (contract v0.7).
//!
//! Invite links carry a per-user code; signups (guest or direct register)
//! record `referred_by`. A referral **qualifies** when the invitee has a real
//! account (not a guest) and has hit the daily reading goal on
//! [`QUALIFY_DAYS`] distinct days — evaluated lazily whenever the referrer
//! opens their referral status, and only while a `referral` event is active.
//! Payout: [`REWARD_DAYS`] of credit-based Pro to BOTH sides. Self-invites
//! from the same IP never pay out (marked rejected).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{random_token, AuthUser};
use crate::db::{self, now_secs};
use crate::error::{AppError, AppJson, AppPath};
use crate::stats::DAILY_GOAL;
use crate::AppState;

/// Distinct goal-hitting days an invitee needs before the payout.
pub const QUALIFY_DAYS: i64 = 3;
/// Pro days granted to each side of a qualified referral (1 month / 1 month).
pub const REWARD_DAYS: i64 = 30;

/// ref_credited states: 0 = pending, 1 = paid out, 2 = rejected (same-IP).
const CREDITED: i64 = 1;
const REJECTED: i64 = 2;

/// GET /api/referral — the caller's invite status; mints the code lazily and
/// runs the qualification sweep (payouts only during an active referral
/// event). Guests get a 403 nudge: credits need an account to stick to.
pub async fn status(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>, AppError> {
    if user.guest {
        return Err(AppError::Status(
            StatusCode::FORBIDDEN,
            "create an account first — invite rewards need somewhere to live".into(),
        ));
    }
    let now = now_secs();
    let fresh = random_token(8);
    let uid = user.id.clone();
    let (code, invited, pending, qualified, event) = state
        .db
        .call(move |c| {
            let code = db::ensure_ref_code(c, &uid, &fresh)?;
            let event = db::active_events(c, Some("referral"), now)?.into_iter().next();
            let my_ip = db::user_signup_ip(c, &uid)?;
            let children = db::referral_children(c, &uid)?;
            let mut invited = 0i64;
            let mut pending = 0i64;
            let mut qualified = 0i64;
            for (child_id, guest, credited, child_ip) in children {
                invited += 1;
                match credited {
                    n if n == CREDITED => qualified += 1,
                    n if n == REJECTED => {}
                    _ => {
                        // Same-IP self-invites are rejected permanently
                        // ("unknown" means we couldn't attribute — no dedup).
                        if let (Some(a), Some(b)) = (&my_ip, &child_ip) {
                            if a == b && a != "unknown" {
                                db::set_ref_credited(c, &child_id, REJECTED)?;
                                continue;
                            }
                        }
                        let done = !guest
                            && db::qualifying_days(c, &child_id, DAILY_GOAL)? >= QUALIFY_DAYS;
                        if done && event.is_some() {
                            db::grant_pro_days(c, &uid, REWARD_DAYS, now)?;
                            db::grant_pro_days(c, &child_id, REWARD_DAYS, now)?;
                            db::set_ref_credited(c, &child_id, CREDITED)?;
                            qualified += 1;
                        } else {
                            pending += 1;
                        }
                    }
                }
            }
            Ok::<_, rusqlite::Error>((code, invited, pending, qualified, event))
        })
        .await?;

    Ok(Json(json!({
        "code": code,
        "path": format!("/r/{code}"),
        "invited": invited,
        "pending": pending,
        "qualified": qualified,
        "reward_days": REWARD_DAYS,
        "qualify_days": QUALIFY_DAYS,
        "event": event.map(|e| json!({"title": e.title, "ends_at": e.ends_at})),
    })))
}

/// GET /api/events/active — public: running events for client banners.
pub async fn active(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let now = now_secs();
    let events = state
        .db
        .call(move |c| db::active_events(c, None, now))
        .await?;
    let out: Vec<Value> = events
        .into_iter()
        .map(|e| json!({"kind": e.kind, "title": e.title, "ends_at": e.ends_at}))
        .collect();
    Ok(Json(Value::Array(out)))
}

// ------------------------------------------------------------- admin API

/// Constant-time bearer check against FLICK_ADMIN_TOKEN (404 when unset so
/// the endpoint's existence isn't advertised).
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(expected) = state.config.admin_token.as_deref() else {
        return Err(AppError::NotFound);
    };
    let given = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let ok = expected.len() == given.len()
        && expected
            .bytes()
            .zip(given.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0;
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

#[derive(Deserialize)]
pub struct EventBody {
    kind: String,
    title: String,
    starts_at: i64,
    ends_at: i64,
    #[serde(default)]
    payload: Option<Value>,
}

const EVENT_KINDS: [&str; 3] = ["referral", "free_pro", "promo"];

/// POST /api/admin/events — start (schedule) a global event.
pub async fn admin_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppJson(body): AppJson<EventBody>,
) -> Result<Response, AppError> {
    require_admin(&state, &headers)?;
    if !EVENT_KINDS.contains(&body.kind.as_str()) {
        return Err(AppError::bad_request("kind must be referral | free_pro | promo"));
    }
    if body.ends_at <= body.starts_at {
        return Err(AppError::bad_request("ends_at must be after starts_at"));
    }
    let event = db::Event {
        id: random_token(8),
        kind: body.kind,
        title: body.title,
        starts_at: body.starts_at,
        ends_at: body.ends_at,
        payload: body
            .payload
            .map(|p| p.to_string())
            .unwrap_or_else(|| "{}".into()),
    };
    let stored = event.clone();
    state.db.call(move |c| db::insert_event(c, &stored)).await?;
    Ok((StatusCode::CREATED, Json(event)).into_response())
}

/// GET /api/admin/events — all events, newest first.
pub async fn admin_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<db::Event>>, AppError> {
    require_admin(&state, &headers)?;
    let events = state.db.call(db::list_events).await?;
    Ok(Json(events))
}

/// DELETE /api/admin/events/{id} — end/remove an event.
pub async fn admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    require_admin(&state, &headers)?;
    let gone = state.db.call(move |c| db::delete_event(c, &id)).await?;
    if gone {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}
