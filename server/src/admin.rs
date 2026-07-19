//! Admin API (CONTRACTS.md "Admin API & panel", v0.10).
//!
//! The admin surface is consumed by the separate flick-admin panel only —
//! bearer auth, no cookies, so the panel can live on its own origin. Two
//! principals: the break-glass `FLICK_ADMIN_TOKEN` and admin *sessions*
//! (password login of an `is_admin` user, sha256-stored, 12 h expiry).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{random_token, sha256_hex, verify_password, DUMMY_HASH};
use crate::db::{self, now_secs};
use crate::error::{AppError, AppJson, AppPath};
use crate::AppState;

const SESSION_TTL: i64 = 12 * 60 * 60;

/// Who is acting: the env token, or a signed-in admin user.
pub enum AdminAuth {
    Token,
    User(Box<db::User>),
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Authorize an /api/admin request. 404 (not 401) when the admin surface is
/// entirely unconfigured — no token env and no admin user — so its existence
/// isn't advertised.
pub async fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<AdminAuth, AppError> {
    let given = bearer(headers).to_string();
    if let Some(expected) = state.config.admin_token.as_deref() {
        if !given.is_empty() && constant_eq(expected, &given) {
            return Ok(AdminAuth::Token);
        }
    }
    if !given.is_empty() {
        let hash = sha256_hex(&given);
        let now = now_secs();
        let user = state
            .db
            .call(move |c| db::admin_session_user(c, &hash, now))
            .await?;
        if let Some(user) = user {
            return Ok(AdminAuth::User(Box::new(user)));
        }
    }
    let configured =
        state.config.admin_token.is_some() || state.db.call(db::any_admin_exists).await?;
    if configured {
        Err(AppError::Unauthorized)
    } else {
        Err(AppError::NotFound)
    }
}

// ------------------------------------------------------------------ login

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

/// POST /api/admin/login — password login for `is_admin` users. Same
/// timing-safe dummy-hash dance as the user login.
pub async fn login(
    State(state): State<AppState>,
    AppJson(body): AppJson<LoginBody>,
) -> Result<Response, AppError> {
    let email = body.email.trim().to_lowercase();
    let user = state.db.call(move |c| db::user_by_email(c, &email)).await?;

    let password = body.password;
    let (user, ok) = tokio::task::spawn_blocking(move || {
        let real = user.as_ref().and_then(|u| u.password_hash.clone());
        let has_password = real.is_some();
        let hash = real.unwrap_or_else(|| DUMMY_HASH.clone());
        let ok = verify_password(&password, &hash) && has_password;
        (user, ok)
    })
    .await
    .map_err(AppError::internal)?;

    match user {
        Some(user) if ok && user.is_admin && !user.guest => {
            let token = random_token(32);
            let hash = sha256_hex(&token);
            let now = now_secs();
            let expires = now + SESSION_TTL;
            let uid = user.id.clone();
            state
                .db
                .call(move |c| db::admin_session_insert(c, &hash, &uid, now, expires))
                .await?;
            Ok(Json(json!({
                "token": token,
                "expires_at": expires,
                "email": user.email,
                "name": user.name,
            }))
            .into_response())
        }
        _ => Err(AppError::Unauthorized),
    }
}

/// DELETE /api/admin/session — revoke the presented session token.
pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, AppError> {
    let hash = sha256_hex(bearer(&headers));
    state
        .db
        .call(move |c| db::admin_session_delete(c, &hash))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// GET /api/admin/me — who am I acting as.
pub async fn me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    match require_admin(&state, &headers).await? {
        AdminAuth::Token => Ok(Json(json!({"via": "token"}))),
        AdminAuth::User(u) => Ok(Json(json!({
            "via": "session",
            "email": u.email,
            "name": u.name,
        }))),
    }
}

// --------------------------------------------------------------- overview

/// GET /api/admin/overview — the dashboard payload in one round trip.
pub async fn overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_admin(&state, &headers).await?;
    let uptime = state.started.elapsed().as_secs();
    let edition = state.config.edition.as_str().to_string();
    let stats = state.db.call(db::admin_overview).await?;
    let mut out = stats;
    out["version"] = json!(env!("CARGO_PKG_VERSION"));
    out["edition"] = json!(edition);
    out["uptime_secs"] = json!(uptime);
    Ok(Json(out))
}

// ------------------------------------------------------------------ users

#[derive(Deserialize)]
pub struct UsersQuery {
    #[serde(default)]
    q: String,
    limit: Option<i64>,
    offset: Option<i64>,
}

/// GET /api/admin/users?q=&limit=&offset=
pub async fn users_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<UsersQuery>,
) -> Result<Json<Value>, AppError> {
    require_admin(&state, &headers).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let q = query.q.trim().to_string();
    let out = state
        .db
        .call(move |c| db::admin_users(c, &q, limit, offset))
        .await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct UserPatch {
    is_admin: Option<bool>,
    plan: Option<String>,
}

/// PATCH /api/admin/users/{id} — flip is_admin / plan.
pub async fn user_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppPath(id): AppPath<String>,
    AppJson(body): AppJson<UserPatch>,
) -> Result<Json<Value>, AppError> {
    let auth = require_admin(&state, &headers).await?;
    let lookup = id.clone();
    let target = state
        .db
        .call(move |c| db::user_by_id(c, &lookup))
        .await?
        .ok_or(AppError::NotFound)?;

    if let Some(is_admin) = body.is_admin {
        if is_admin && (target.guest || target.email.is_none()) {
            return Err(AppError::bad_request(
                "only registered accounts with an email can be admins",
            ));
        }
        // A session admin locking themselves out is almost always a mistake;
        // the env token may still demote anyone.
        if !is_admin {
            if let AdminAuth::User(me) = &auth {
                if me.id == target.id {
                    return Err(AppError::bad_request("you cannot demote yourself"));
                }
            }
        }
        let uid = id.clone();
        state
            .db
            .call(move |c| db::user_set_admin(c, &uid, is_admin))
            .await?;
    }
    if let Some(plan) = body.plan {
        if plan != "free" && plan != "pro" {
            return Err(AppError::bad_request("plan must be free | pro"));
        }
        let uid = id.clone();
        state
            .db
            .call(move |c| db::user_set_plan(c, &uid, &plan))
            .await?;
    }
    let user = state
        .db
        .call(move |c| db::user_by_id(c, &id))
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "guest": user.guest,
        "is_admin": user.is_admin,
        "plan": user.plan,
    })))
}

/// DELETE /api/admin/users/{id} — full cascade delete.
pub async fn user_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppPath(id): AppPath<String>,
) -> Result<StatusCode, AppError> {
    let auth = require_admin(&state, &headers).await?;
    if let AdminAuth::User(me) = &auth {
        if me.id == id {
            return Err(AppError::bad_request("you cannot delete yourself"));
        }
    }
    let lookup = id.clone();
    let target = state
        .db
        .call(move |c| db::user_by_id(c, &lookup))
        .await?
        .ok_or(AppError::NotFound)?;
    state
        .db
        .call(move |c| db::delete_user(c, &id, target.email.as_deref()))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ----------------------------------------------------------- announcement

/// GET /api/admin/announcement
pub async fn announcement_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_admin(&state, &headers).await?;
    let row = state.db.call(db::announcement_get).await?;
    Ok(Json(row))
}

#[derive(Deserialize)]
pub struct AnnouncementBody {
    #[serde(default)]
    text: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    active: bool,
}

/// PUT /api/admin/announcement — publish/replace the user-site banner.
pub async fn announcement_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppJson(body): AppJson<AnnouncementBody>,
) -> Result<Json<Value>, AppError> {
    require_admin(&state, &headers).await?;
    if body.active && body.text.trim().is_empty() {
        return Err(AppError::bad_request("an active announcement needs text"));
    }
    if body.text.len() > 300 || body.link.len() > 300 || body.label.len() > 60 {
        return Err(AppError::bad_request("announcement too long"));
    }
    let now = now_secs();
    state
        .db
        .call(move |c| {
            db::announcement_put(c, &body.text, &body.link, &body.label, body.active, now)
        })
        .await?;
    let row = state.db.call(db::announcement_get).await?;
    Ok(Json(row))
}
