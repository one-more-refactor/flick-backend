//! Local auth: register/login/logout/me, guest sessions, the email-first
//! lookup + 6-digit login codes, argon2id password hashing, `flick_session`
//! cookie sessions, guest-merge on auth success, and the `AuthUser` extractor.

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
use sha2::{Digest, Sha256};

use crate::db::{self, now_secs, Db, User};
use crate::error::{AppError, AppJson};
use crate::AppState;

pub const SESSION_COOKIE: &str = "flick_session";
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 60 * 60; // 30 days

const LOGIN_CODE_TTL_SECS: i64 = 10 * 60;
const LOGIN_CODE_MAX_ATTEMPTS: i64 = 5;

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

fn sha256_hex(input: &str) -> String {
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Pseudonymize a client IP before storing it for referral same-IP dedup, so
/// no raw IP address (personal data) is retained. `"unknown"` (unattributable)
/// is kept verbatim so the dedup can skip it. Both sides of a comparison are
/// stored the same way, so hashing is transparent to the dedup check.
fn hash_ip(ip: &str) -> String {
    if ip == "unknown" {
        ip.to_string()
    } else {
        sha256_hex(&format!("flick-ip-v1:{ip}"))
    }
}

/// Constant-time equality so code verification can't be timed byte-by-byte.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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

/// The contract user object. Async because `"uploads"` carries the live
/// weekly counter (CONTRACTS.md "Editions & plans": `limit` is 15 on the
/// hosted free plan, `null` = unlimited otherwise) — every auth response
/// that returns user JSON goes through here.
pub async fn user_json(state: &AppState, user: &User) -> Result<Value, AppError> {
    let uid = user.id.clone();
    let now = now_secs();
    let used = state
        .db
        .call(move |c| db::uploads_this_week(c, &uid, now))
        .await?;
    let pro = crate::books::pro_active(state, user).await?;
    let limit = crate::books::weekly_upload_limit(&state.config, user, pro);
    let pro_days = if user.pro_until > now {
        (user.pro_until - now).div_euclid(86_400) + 1
    } else {
        0
    };
    Ok(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "username": user.username,
        "guest": user.guest,
        "onboarded": user.onboarded,
        "plan": user.plan,
        "pro_active": pro,
        "pro_days": pro_days,
        "avatar": user.avatar,
        "settings": {
            "wpm": user.wpm,
            "theme": user.theme,
            "accent": user.accent,
            "lang": user.lang,
        },
        "uploads": {"used": used, "limit": limit},
    }))
}

/// A brand-new user with contract defaults (onboarded=false, 350 wpm, auto
/// theme, red accent, auto lang).
pub fn new_user(
    email: Option<String>,
    name: String,
    password_hash: Option<String>,
    guest: bool,
) -> User {
    User {
        id: random_token(16),
        email,
        name,
        password_hash,
        username: None,
        onboarded: false,
        wpm: 350,
        theme: "auto".into(),
        guest,
        accent: "red".into(),
        lang: "auto".into(),
        plan: "free".into(),
        pro_until: 0,
        avatar: None,
    }
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

/// Contract: when register/login succeeds (any method, OAuth included) while
/// the request still carries a valid session of a *guest* user, that guest's
/// library and stats merge into the target account. Every auth success path
/// calls this before issuing the new session cookie.
pub async fn merge_guest_from_request(
    state: &AppState,
    headers: &HeaderMap,
    target_id: &str,
) -> Result<(), AppError> {
    let Some(token) = cookie_value(headers, SESSION_COOKIE) else {
        return Ok(());
    };
    let now = now_secs();
    let target = target_id.to_string();
    state
        .db
        .call(move |c| {
            if let Some(user) = db::session_user(c, &token, now)? {
                if user.guest {
                    db::merge_guest_into(c, &user.id, &target)?;
                }
            }
            Ok(())
        })
        .await
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

/// POST /api/auth/guest — anonymous user row + session (no intro book; that
/// is seeded alongside the guest's first own add, see books.rs).
#[derive(Deserialize, Default)]
pub struct GuestBody {
    #[serde(rename = "ref")]
    ref_code: Option<String>,
}

pub async fn guest(
    State(state): State<AppState>,
    ip: crate::ratelimit::ClientIp,
    body: axum::body::Bytes,
) -> Result<Response, AppError> {
    let user = new_user(None, "READER".into(), None, true);
    let now = now_secs();
    let stored = user.clone();
    // Optional {"ref": code} body — absent bodies are fine.
    let ref_code = serde_json::from_slice::<GuestBody>(&body)
        .ok()
        .and_then(|b| b.ref_code)
        .filter(|c| !c.is_empty());
    let ip = hash_ip(&ip.0);
    state
        .db
        .call(move |c| {
            db::insert_user(c, &stored, now)?;
            if let Some(code) = ref_code {
                if let Some(referrer) = db::user_id_by_ref_code(c, &code)? {
                    db::set_referred_by(c, &stored.id, &referrer, &ip)?;
                }
            }
            // Contract "Starter library": no library ever starts empty.
            crate::catalog::seed_default_library(c, &stored.id, now)
        })
        .await?;
    let body = user_json(&state, &user).await?;
    start_session(&state, &user.id, StatusCode::CREATED, body).await
}

#[derive(Deserialize)]
pub struct LookupBody {
    email: String,
}

/// POST /api/auth/lookup — the email-first flow's fork: does the account
/// exist, and which sign-in methods does it have?
pub async fn lookup(
    State(state): State<AppState>,
    AppJson(body): AppJson<LookupBody>,
) -> Result<Json<Value>, AppError> {
    let email = body.email.trim().to_lowercase();
    let methods = state
        .db
        .call(move |c| {
            let Some(user) = db::user_by_email(c, &email)? else {
                return Ok(None);
            };
            let mut methods = Vec::new();
            if user.password_hash.is_some() {
                methods.push("password".to_string());
            }
            methods.push("code".to_string()); // email code always works
            methods.extend(db::identity_providers(c, &user.id)?);
            Ok(Some(methods))
        })
        .await?;
    Ok(Json(json!({
        "exists": methods.is_some(),
        "methods": methods.unwrap_or_default(),
    })))
}

/// Generate, store (hashed) and return a fresh 6-digit login code for
/// `email`. Public so tests can mint a known code — the HTTP handler never
/// reveals it (SMTP or, in dev mode, the log does). Codes are short-lived
/// single-use 6-digit secrets, so sha256 is plenty (argon2 unnecessary).
pub async fn issue_login_code(db: &Db, email: &str) -> Result<String, AppError> {
    let code = format!("{:06}", rand::rngs::OsRng.next_u32() % 1_000_000);
    let code_hash = sha256_hex(&code);
    let expires_at = now_secs() + LOGIN_CODE_TTL_SECS;
    let email = email.to_string();
    db.call(move |c| db::upsert_login_code(c, &email, &code_hash, expires_at))
        .await?;
    Ok(code)
}

#[derive(Deserialize)]
pub struct CodeRequestBody {
    email: String,
}

/// POST /api/auth/code/request — always 204 (never reveals whether the
/// account exists). Existing accounts get a code by mail, or in the server
/// log when FLICK_SMTP_URL is unset (dev mode).
pub async fn code_request(
    State(state): State<AppState>,
    AppJson(body): AppJson<CodeRequestBody>,
) -> Result<StatusCode, AppError> {
    let email = body.email.trim().to_lowercase();
    let lookup = email.clone();
    let exists = state
        .db
        .call(move |c| db::user_by_email(c, &lookup))
        .await?
        .is_some();
    if exists {
        let code = issue_login_code(&state.db, &email).await?;
        if state.config.smtp_url.is_some() {
            if let Err(e) = crate::mail::send_login_code(&state.config, &email, &code).await {
                // Still 204: a mail failure must not reveal account existence.
                tracing::error!("login code mail to {email} failed: {e:?}");
            }
        } else {
            tracing::info!(%email, %code, "login code (dev mode: FLICK_SMTP_URL unset)");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct CodeVerifyBody {
    email: String,
    code: String,
}

/// POST /api/auth/code/verify — single-use, 5 attempts, 10-min expiry.
/// Unknown email, wrong code and expired code are the same 400.
pub async fn code_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppJson(body): AppJson<CodeVerifyBody>,
) -> Result<Response, AppError> {
    let email = body.email.trim().to_lowercase();
    let presented_hash = sha256_hex(body.code.trim());
    let now = now_secs();
    let user = state
        .db
        .call(move |c| {
            let Some((code_hash, expires_at, attempts)) = db::login_code(c, &email)? else {
                return Ok(None);
            };
            if expires_at < now || attempts >= LOGIN_CODE_MAX_ATTEMPTS {
                db::delete_login_code(c, &email)?;
                return Ok(None);
            }
            db::bump_login_code_attempts(c, &email)?;
            if !ct_eq(code_hash.as_bytes(), presented_hash.as_bytes()) {
                if attempts + 1 >= LOGIN_CODE_MAX_ATTEMPTS {
                    db::delete_login_code(c, &email)?;
                }
                return Ok(None);
            }
            db::delete_login_code(c, &email)?; // single-use
            db::user_by_email(c, &email) // codes only log into EXISTING accounts
        })
        .await?;
    let Some(user) = user else {
        return Err(AppError::bad_request("invalid code"));
    };
    merge_guest_from_request(&state, &headers, &user.id).await?;
    let body = user_json(&state, &user).await?;
    start_session(&state, &user.id, StatusCode::OK, body).await
}

#[derive(Deserialize)]
pub struct RegisterBody {
    email: String,
    password: String,
    name: Option<String>,
    #[serde(rename = "ref", default)]
    ref_code: Option<String>,
}

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    ip: crate::ratelimit::ClientIp,
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
    // Contract: name is optional — default to the email's local part.
    let name = body
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .or_else(|| email.split('@').next().map(str::to_string))
        .unwrap_or_else(|| "reader".into());

    let body_ref = body.ref_code.clone();
    let password = body.password;
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(AppError::internal)??;

    let user = new_user(Some(email), name, Some(password_hash), false);
    let now = now_secs();
    let ref_code = body_ref.filter(|c| !c.is_empty());
    let ip = hash_ip(&ip.0);
    let inserted = state
        .db
        .call({
            let user = user.clone();
            move |c| {
                if db::user_by_email(c, user.email.as_deref().unwrap_or_default())?.is_some() {
                    return Ok(false);
                }
                db::insert_user(c, &user, now)?;
                if let Some(code) = &ref_code {
                    if let Some(referrer) = db::user_id_by_ref_code(c, code)? {
                        db::set_referred_by(c, &user.id, &referrer, &ip)?;
                    }
                }
                crate::catalog::seed_default_library(c, &user.id, now)?;
                Ok(true)
            }
        })
        .await?;
    if !inserted {
        return Err(AppError::Conflict(
            "an account with this email already exists".into(),
        ));
    }

    merge_guest_from_request(&state, &headers, &user.id).await?;
    let body = user_json(&state, &user).await?;
    start_session(&state, &user.id, StatusCode::CREATED, body).await
}

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
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
            merge_guest_from_request(&state, &headers, &user.id).await?;
            let body = user_json(&state, &user).await?;
            start_session(&state, &user.id, StatusCode::OK, body).await
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

pub async fn me(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>, AppError> {
    Ok(Json(user_json(&state, &user).await?))
}

/// GET /api/auth/export — a full JSON download of the caller's own data
/// (GDPR access/portability). Streams as an attachment.
pub async fn export_me(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Response, AppError> {
    let u = user.clone();
    let data = state.db.call(move |c| db::export_user(c, &u)).await?;
    let body = serde_json::to_vec_pretty(&data).map_err(AppError::internal)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"flick-export.json\"",
            ),
        ],
        body,
    )
        .into_response())
}

/// DELETE /api/auth/me — permanently erase the account and everything it owns
/// (GDPR erasure), then clear the session cookie. Irreversible.
pub async fn delete_me(
    State(state): State<AppState>,
    AuthUser(user): AuthUser,
) -> Result<Response, AppError> {
    let email = user.email.clone();
    let uid = user.id.clone();
    state
        .db
        .call(move |c| db::delete_user(c, &uid, email.as_deref()))
        .await?;
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_session_cookie(state.config.cookie_secure()))
            .map_err(AppError::internal)?,
    );
    Ok(resp)
}

#[derive(Deserialize)]
pub struct SettingsPatch {
    wpm: Option<i64>,
    theme: Option<String>,
    accent: Option<String>,
    lang: Option<String>,
}

#[derive(Deserialize)]
pub struct MePatch {
    username: Option<String>,
    name: Option<String>,
    onboarded: Option<bool>,
    /// Square profile picture: a `data:image/...;base64,...` URL to set, or an
    /// empty string to clear it. Absent = unchanged.
    avatar: Option<String>,
    settings: Option<SettingsPatch>,
}

/// A profile picture must be a small, self-contained `data:` image URL. Cap the
/// stored size (~150 KB of base64) so the DB and every `user_json` stay light.
const MAX_AVATAR_LEN: usize = 200_000;

fn valid_avatar(data: &str) -> bool {
    data.len() <= MAX_AVATAR_LEN
        && data.starts_with("data:image/")
        && data.contains(";base64,")
}

fn valid_username(u: &str) -> bool {
    (2..=24).contains(&u.len())
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub async fn update_me(
    State(state): State<AppState>,
    AuthUser(mut user): AuthUser,
    AppJson(patch): AppJson<MePatch>,
) -> Result<Response, AppError> {
    if let Some(username) = patch.username {
        let username = username.trim().to_string();
        if !valid_username(&username) {
            return Err(AppError::bad_request(
                "username must be 2-24 characters: letters, digits, _ or -",
            ));
        }
        user.username = Some(username);
    }
    if let Some(name) = patch.name {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(AppError::bad_request("name must not be empty"));
        }
        user.name = name;
    }
    if let Some(onboarded) = patch.onboarded {
        user.onboarded = onboarded;
    }
    if let Some(avatar) = patch.avatar {
        if avatar.is_empty() {
            user.avatar = None; // explicit clear
        } else if valid_avatar(&avatar) {
            user.avatar = Some(avatar);
        } else {
            return Err(AppError::bad_request(
                "avatar must be a data:image URL under 150 KB",
            ));
        }
    }
    if let Some(settings) = patch.settings {
        if let Some(wpm) = settings.wpm {
            if !(100..=1200).contains(&wpm) {
                return Err(AppError::bad_request("wpm must be between 100 and 1200"));
            }
            user.wpm = wpm;
        }
        if let Some(theme) = settings.theme {
            if !matches!(theme.as_str(), "auto" | "light" | "dark") {
                return Err(AppError::bad_request("theme must be auto, light, or dark"));
            }
            user.theme = theme;
        }
        if let Some(accent) = settings.accent {
            if !matches!(
                accent.as_str(),
                "red" | "ember" | "acid" | "cyan" | "violet" | "mono"
            ) {
                return Err(AppError::bad_request(
                    "accent must be red, ember, acid, cyan, violet, or mono",
                ));
            }
            user.accent = accent;
        }
        if let Some(lang) = settings.lang {
            if !matches!(lang.as_str(), "auto" | "en" | "de" | "es") {
                return Err(AppError::bad_request("lang must be auto, en, de, or es"));
            }
            user.lang = lang;
        }
    }

    let stored = user.clone();
    state
        .db
        .call(move |c| db::update_profile(c, &stored))
        .await?;
    Ok(Json(user_json(&state, &user).await?).into_response())
}

/// GET /api/auth/providers — the configured sign-in providers only.
pub async fn providers(State(state): State<AppState>) -> Json<Value> {
    let mut providers = Vec::new();
    if state.config.oidc.is_some() {
        providers.push(json!({"id": "oidc", "name": state.config.oidc_name}));
    }
    if state.config.oauth_google.is_some() {
        providers.push(json!({"id": "google", "name": "Google"}));
    }
    if state.config.oauth_github.is_some() {
        providers.push(json!({"id": "github", "name": "GitHub"}));
    }
    Json(json!({ "providers": providers }))
}
