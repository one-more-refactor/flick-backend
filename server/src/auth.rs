//! Local auth: register/login/logout/me, argon2id password hashing,
//! `flick_session` cookie sessions, and the `AuthUser` extractor.

use std::sync::LazyLock;

use argon2::password_hash::rand_core::OsRng as SaltRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::db::{self, now_secs, User};
use crate::error::{AppError, AppJson};
use crate::AppState;

pub const SESSION_COOKIE: &str = "flick_session";
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60; // 30 days

/// Hash of a throwaway password, verified when the user doesn't exist so
/// login latency doesn't reveal whether an email is registered.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| {
    // Infallible in practice: fixed input, default params.
    hash_password("flick-dummy-password-for-timing").expect("argon2 dummy hash")
});

// ------------------------------------------------------------- utilities

pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut SaltRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(AppError::internal)
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// Extract a cookie value from request headers.
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}{}",
        if secure { "; Secure" } else { "" }
    )
}

fn clear_session_cookie(secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn user_json(user: &User) -> Value {
    json!({ "id": user.id, "email": user.email, "name": user.name })
}

/// Create a session row and return a response carrying the session cookie.
pub async fn start_session(
    state: &AppState,
    user_id: &str,
    status: StatusCode,
    body: Value,
) -> Result<Response, AppError> {
    let token = random_token(32);
    let now = now_secs();
    let uid = user_id.to_string();
    let t = token.clone();
    state
        .db
        .call(move |c| db::create_session(c, &t, &uid, now, now + SESSION_TTL_SECS))
        .await?;
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&token, state.config.cookie_secure()))
            .map_err(AppError::internal)?,
    );
    Ok(resp)
}

// -------------------------------------------------------------- extractor

/// The authenticated user, resolved from the `flick_session` cookie.
pub struct AuthUser(pub User);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token =
            cookie_value(&parts.headers, SESSION_COOKIE).ok_or(AppError::Unauthorized)?;
        let now = now_secs();
        let user = state
            .db
            .call(move |c| db::session_user(c, &token, now))
            .await?
            .ok_or(AppError::Unauthorized)?;
        Ok(AuthUser(user))
    }
}

// --------------------------------------------------------------- handlers

#[derive(Deserialize)]
pub struct RegisterBody {
    email: String,
    password: String,
    name: String,
}

pub async fn register(
    State(state): State<AppState>,
    AppJson(body): AppJson<RegisterBody>,
) -> Result<Response, AppError> {
    let email = body.email.trim().to_lowercase();
    if email.len() < 3 || !email.contains('@') {
        return Err(AppError::bad_request("invalid email address"));
    }
    if body.password.len() < 8 {
        return Err(AppError::bad_request(
            "password must be at least 8 characters",
        ));
    }
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::bad_request("name is required"));
    }

    let password = body.password;
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(AppError::internal)??;

    let user = User {
        id: random_token(16),
        email,
        name,
        password_hash: Some(password_hash),
    };
    let now = now_secs();
    let inserted = state
        .db
        .call({
            let user = user.clone();
            move |c| {
                if db::user_by_email(c, &user.email)?.is_some() {
                    return Ok(false);
                }
                db::insert_user(c, &user, None, now)?;
                Ok(true)
            }
        })
        .await?;
    if !inserted {
        return Err(AppError::Conflict(
            "an account with this email already exists".into(),
        ));
    }

    start_session(&state, &user.id, StatusCode::CREATED, user_json(&user)).await
}

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    AppJson(body): AppJson<LoginBody>,
) -> Result<Response, AppError> {
    let email = body.email.trim().to_lowercase();
    let user = state
        .db
        .call(move |c| db::user_by_email(c, &email))
        .await?;

    // Always verify against some argon2 hash so response timing doesn't
    // reveal whether the email exists (or is an SSO-only account).
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
        Some(user) if ok => {
            start_session(&state, &user.id, StatusCode::OK, user_json(&user)).await
        }
        _ => Err(AppError::Status(
            StatusCode::UNAUTHORIZED,
            "invalid email or password".into(),
        )),
    }
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        state
            .db
            .call(move |c| db::delete_session(c, &token))
            .await?;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_session_cookie(state.config.cookie_secure()))
            .map_err(AppError::internal)?,
    );
    Ok(resp)
}

pub async fn me(AuthUser(user): AuthUser) -> Json<Value> {
    Json(user_json(&user))
}

pub async fn providers(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "oidc": {
            "enabled": state.config.oidc.is_some(),
            "name": state.config.oidc_name,
        }
    }))
}
