//! OAuth sign-in: a small provider registry behind
//! `GET /api/auth/oauth/{provider}/login|callback`.
//!
//! - `oidc`   — generic OIDC SSO (Authentik first), FLICK_OIDC_* env
//! - `google` — Google via the same OIDC machinery (fixed issuer),
//!   FLICK_OAUTH_GOOGLE_* env
//! - `github` — plain OAuth2 (GitHub speaks no OIDC), FLICK_OAUTH_GITHUB_* env
//!
//! `/api/auth/oidc/login|callback` remain as aliases for the generic
//! provider — the alias callback is also the redirect URI we register, so
//! existing IdP client configs keep working. Every provider finds-or-creates
//! through the `identities` table, links to an existing account on a
//! verified-email match, and merges a guest session's library on success.
//!
//! Providers are enabled only when their env creds are set. OIDC discovery
//! is lazy (first use) so the server starts fine when an issuer is
//! unreachable; discovery failures surface as 502s on these routes only.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreProviderMetadata,
};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::auth::{
    merge_guest_from_request, new_user, random_token, start_session, user_json,
};
use crate::config::Config;
use crate::db::{self, now_secs};
use crate::error::{AppError, AppPath};
use crate::AppState;

const STATE_TTL_SECS: i64 = 600; // 10 minutes

const GOOGLE_ISSUER: &str = "https://accounts.google.com";
const GITHUB_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_API_USER: &str = "https://api.github.com/user";
const GITHUB_API_EMAILS: &str = "https://api.github.com/user/emails";
/// GitHub rejects requests without a User-Agent.
const USER_AGENT: &str = "flick-server";

type Client = CoreClient<
    EndpointSet,      // auth url (from discovery)
    EndpointNotSet,   // device auth
    EndpointNotSet,   // introspection
    EndpointNotSet,   // revocation
    EndpointMaybeSet, // token url
    EndpointMaybeSet, // userinfo url
>;

/// Lazily-discovered OIDC clients (generic + Google), shared across requests.
#[derive(Default)]
pub struct OauthRuntime {
    oidc: OnceCell<(Client, openidconnect::reqwest::Client)>,
    google: OnceCell<(Client, openidconnect::reqwest::Client)>,
}

impl OauthRuntime {
    async fn oidc_client(
        &self,
        config: &Config,
        provider: &str,
    ) -> Result<&(Client, openidconnect::reqwest::Client), AppError> {
        let (cell, issuer, client_id, client_secret) = match provider {
            "oidc" => {
                let s = config.oidc.as_ref().ok_or(AppError::NotFound)?;
                (&self.oidc, s.issuer.clone(), s.client_id.clone(), s.client_secret.clone())
            }
            "google" => {
                let s = config.oauth_google.as_ref().ok_or(AppError::NotFound)?;
                (
                    &self.google,
                    GOOGLE_ISSUER.to_string(),
                    s.client_id.clone(),
                    s.client_secret.clone(),
                )
            }
            _ => return Err(AppError::NotFound),
        };
        let redirect = callback_url(config, provider);
        cell.get_or_try_init(|| async {
            let http = openidconnect::reqwest::ClientBuilder::new()
                // Required by openidconnect: never follow redirects.
                .redirect(openidconnect::reqwest::redirect::Policy::none())
                .build()
                .map_err(AppError::internal)?;
            let issuer = IssuerUrl::new(issuer)
                .map_err(|e| AppError::Internal(format!("bad OIDC issuer: {e}")))?;
            let metadata = CoreProviderMetadata::discover_async(issuer, &http)
                .await
                .map_err(|e| {
                    AppError::Status(
                        StatusCode::BAD_GATEWAY,
                        format!("OIDC provider discovery failed: {e}"),
                    )
                })?;
            let redirect = RedirectUrl::new(redirect).map_err(AppError::internal)?;
            let client = CoreClient::from_provider_metadata(
                metadata,
                ClientId::new(client_id),
                Some(ClientSecret::new(client_secret)),
            )
            .set_redirect_uri(redirect);
            Ok((client, http))
        })
        .await
    }
}

/// The registered redirect URI per provider. The generic provider keeps the
/// legacy `/api/auth/oidc/callback` path (the alias route) so existing IdP
/// registrations stay valid.
fn callback_url(config: &Config, provider: &str) -> String {
    match provider {
        "oidc" => format!("{}/api/auth/oidc/callback", config.public_url),
        p => format!("{}/api/auth/oauth/{p}/callback", config.public_url),
    }
}

// ------------------------------------------------------- state cookies

fn state_cookie_name(provider: &str) -> String {
    format!("flick_oauth_{provider}")
}

/// Path=/api/auth covers both `/api/auth/oauth/*` and the `/api/auth/oidc/*`
/// aliases.
fn state_cookie(provider: &str, value: &str, secure: bool) -> String {
    format!(
        "{}={value}; Path=/api/auth; HttpOnly; SameSite=Lax; Max-Age={STATE_TTL_SECS}{}",
        state_cookie_name(provider),
        if secure { "; Secure" } else { "" }
    )
}

fn clear_state_cookie(provider: &str, secure: bool) -> String {
    format!(
        "{}=; Path=/api/auth; HttpOnly; SameSite=Lax; Max-Age=0{}",
        state_cookie_name(provider),
        if secure { "; Secure" } else { "" }
    )
}

fn redirect_with_cookie(location: &str, cookie: &str) -> Result<Response, AppError> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location)
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .map_err(AppError::internal)
}

fn callback_query(uri: &Uri) -> Result<std::collections::HashMap<String, String>, AppError> {
    let query: std::collections::HashMap<String, String> = Query::try_from_uri(uri)
        .map(|Query(q)| q)
        .map_err(|e| AppError::bad_request(e.body_text()))?;
    if let Some(err) = query.get("error") {
        let desc = query
            .get("error_description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        return Err(AppError::bad_request(format!("OAuth error: {err}{desc}")));
    }
    Ok(query)
}

// ------------------------------------------------------------- routing

/// GET /api/auth/oauth/{provider}/login — 302 to the provider
/// (404 unknown/unconfigured).
pub async fn login(
    State(state): State<AppState>,
    AppPath(provider): AppPath<String>,
) -> Result<Response, AppError> {
    match provider.as_str() {
        "oidc" | "google" => oidc_login(&state, &provider).await,
        "github" => github_login(&state).await,
        _ => Err(AppError::NotFound),
    }
}

/// GET /api/auth/oauth/{provider}/callback — code exchange,
/// find-or-create-or-link user, guest merge, session, 302 /.
pub async fn callback(
    State(state): State<AppState>,
    AppPath(provider): AppPath<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, AppError> {
    match provider.as_str() {
        "oidc" | "google" => oidc_callback(&state, &provider, &headers, &uri).await,
        "github" => github_callback(&state, &headers, &uri).await,
        _ => Err(AppError::NotFound),
    }
}

/// GET /api/auth/oidc/login — alias for the generic provider.
pub async fn oidc_login_alias(State(state): State<AppState>) -> Result<Response, AppError> {
    oidc_login(&state, "oidc").await
}

/// GET /api/auth/oidc/callback — alias for the generic provider (and its
/// registered redirect URI).
pub async fn oidc_callback_alias(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, AppError> {
    oidc_callback(&state, "oidc", &headers, &uri).await
}

// ---------------------------------------------------------- OIDC flavor

async fn oidc_login(state: &AppState, provider: &str) -> Result<Response, AppError> {
    let (client, _) = state.oauth.oidc_client(&state.config, provider).await?;

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    // state | nonce | pkce verifier — all URL-safe, '|' cannot occur.
    let payload = format!(
        "{}|{}|{}",
        csrf.secret(),
        nonce.secret(),
        pkce_verifier.secret()
    );
    let cookie = state_cookie(provider, &payload, state.config.cookie_secure());
    redirect_with_cookie(auth_url.as_str(), &cookie)
}

async fn oidc_callback(
    state: &AppState,
    provider: &str,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Response, AppError> {
    let (client, http) = state.oauth.oidc_client(&state.config, provider).await?;

    let query = callback_query(uri)?;
    let code = query
        .get("code")
        .ok_or_else(|| AppError::bad_request("missing OAuth code parameter"))?;
    let returned_state = query
        .get("state")
        .ok_or_else(|| AppError::bad_request("missing OAuth state parameter"))?;

    let cookie = crate::auth::cookie_value(headers, &state_cookie_name(provider))
        .ok_or_else(|| AppError::bad_request("missing or expired OAuth state cookie"))?;
    let mut parts = cookie.splitn(3, '|');
    let (Some(saved_state), Some(nonce), Some(verifier)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(AppError::bad_request("malformed OAuth state cookie"));
    };
    if saved_state != returned_state {
        return Err(AppError::bad_request("OAuth state mismatch"));
    }

    let token_response = client
        .exchange_code(AuthorizationCode::new(code.clone()))
        .map_err(|e| AppError::Internal(format!("OIDC token endpoint unavailable: {e}")))?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier.to_string()))
        .request_async(http)
        .await
        .map_err(|e| {
            AppError::Status(
                StatusCode::BAD_GATEWAY,
                format!("OIDC code exchange failed: {e}"),
            )
        })?;

    let id_token = token_response.id_token().ok_or_else(|| {
        AppError::Status(
            StatusCode::BAD_GATEWAY,
            "OIDC provider returned no ID token".into(),
        )
    })?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(nonce.to_string()))
        .map_err(|e| AppError::bad_request(format!("invalid ID token: {e}")))?;

    let sub = claims.subject().as_str().to_string();
    let email = claims
        .email()
        .map(|e| e.as_str().trim().to_lowercase())
        .filter(|e| !e.is_empty());
    let email_verified = claims.email_verified().unwrap_or(false);
    let name = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.as_str().to_string())
        .or_else(|| {
            claims
                .preferred_username()
                .map(|u| u.as_str().to_string())
        })
        .or_else(|| {
            email
                .as_deref()
                .and_then(|e| e.split('@').next())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "user".into());

    complete(state, headers, provider, sub, email, email_verified, name).await
}

// -------------------------------------------------------- GitHub flavor

async fn github_login(state: &AppState) -> Result<Response, AppError> {
    let creds = state.config.oauth_github.as_ref().ok_or(AppError::NotFound)?;
    let token = random_token(16);
    let mut url = openidconnect::url::Url::parse(GITHUB_AUTHORIZE_URL)
        .map_err(AppError::internal)?;
    url.query_pairs_mut()
        .append_pair("client_id", &creds.client_id)
        .append_pair("redirect_uri", &callback_url(&state.config, "github"))
        .append_pair("scope", "read:user user:email")
        .append_pair("state", &token);
    let cookie = state_cookie("github", &token, state.config.cookie_secure());
    redirect_with_cookie(url.as_str(), &cookie)
}

async fn github_get(
    http: &openidconnect::reqwest::Client,
    url: &str,
    access_token: &str,
) -> Result<Value, AppError> {
    let resp = http
        .get(url)
        .header("authorization", format!("Bearer {access_token}"))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| {
            AppError::Status(StatusCode::BAD_GATEWAY, format!("GitHub API unreachable: {e}"))
        })?;
    if !resp.status().is_success() {
        return Err(AppError::Status(
            StatusCode::BAD_GATEWAY,
            format!("GitHub API {url} returned {}", resp.status()),
        ));
    }
    let text = resp.text().await.map_err(|e| {
        AppError::Status(StatusCode::BAD_GATEWAY, format!("GitHub API read failed: {e}"))
    })?;
    serde_json::from_str(&text).map_err(|e| {
        AppError::Status(StatusCode::BAD_GATEWAY, format!("GitHub API bad JSON: {e}"))
    })
}

async fn github_callback(
    state: &AppState,
    headers: &HeaderMap,
    uri: &Uri,
) -> Result<Response, AppError> {
    let creds = state.config.oauth_github.as_ref().ok_or(AppError::NotFound)?;

    let query = callback_query(uri)?;
    let code = query
        .get("code")
        .ok_or_else(|| AppError::bad_request("missing OAuth code parameter"))?;
    let returned_state = query
        .get("state")
        .ok_or_else(|| AppError::bad_request("missing OAuth state parameter"))?;
    let cookie = crate::auth::cookie_value(headers, &state_cookie_name("github"))
        .ok_or_else(|| AppError::bad_request("missing or expired OAuth state cookie"))?;
    if cookie != *returned_state {
        return Err(AppError::bad_request("OAuth state mismatch"));
    }

    let http = openidconnect::reqwest::ClientBuilder::new()
        .user_agent(USER_AGENT)
        .build()
        .map_err(AppError::internal)?;

    // Token exchange: form-encoded POST, JSON response via Accept header.
    let body = openidconnect::url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &creds.client_id)
        .append_pair("client_secret", &creds.client_secret)
        .append_pair("code", code)
        .append_pair("redirect_uri", &callback_url(&state.config, "github"))
        .finish();
    let resp = http
        .post(GITHUB_TOKEN_URL)
        .header("accept", "application/json")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| {
            AppError::Status(
                StatusCode::BAD_GATEWAY,
                format!("GitHub token endpoint unreachable: {e}"),
            )
        })?;
    let text = resp.text().await.map_err(|e| {
        AppError::Status(StatusCode::BAD_GATEWAY, format!("GitHub token read failed: {e}"))
    })?;
    let token_json: Value = serde_json::from_str(&text).map_err(|e| {
        AppError::Status(StatusCode::BAD_GATEWAY, format!("GitHub token bad JSON: {e}"))
    })?;
    let access_token = token_json["access_token"].as_str().ok_or_else(|| {
        AppError::Status(
            StatusCode::BAD_GATEWAY,
            format!(
                "GitHub code exchange failed: {}",
                token_json["error_description"]
                    .as_str()
                    .or(token_json["error"].as_str())
                    .unwrap_or("no access token")
            ),
        )
    })?;

    let profile = github_get(&http, GITHUB_API_USER, access_token).await?;
    let sub = profile["id"]
        .as_i64()
        .ok_or_else(|| {
            AppError::Status(StatusCode::BAD_GATEWAY, "GitHub /user returned no id".into())
        })?
        .to_string();
    let name = profile["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| profile["login"].as_str().map(str::to_string))
        .unwrap_or_else(|| "user".into());

    // Primary verified email preferred, any verified as fallback.
    let emails = github_get(&http, GITHUB_API_EMAILS, access_token).await?;
    let email = emails.as_array().and_then(|list| {
        list.iter()
            .find(|e| e["primary"] == true && e["verified"] == true)
            .or_else(|| list.iter().find(|e| e["verified"] == true))
            .and_then(|e| e["email"].as_str())
            .map(|e| e.trim().to_lowercase())
    });
    let email_verified = email.is_some();

    complete(state, headers, "github", sub, email, email_verified, name).await
}

// ------------------------------------------------------ shared finish

/// Find-or-create via `identities`, link on verified-email match, seed the
/// intro book for brand-new users, merge a guest session, start the session.
async fn complete(
    state: &AppState,
    headers: &HeaderMap,
    provider: &str,
    sub: String,
    email: Option<String>,
    email_verified: bool,
    name: String,
) -> Result<Response, AppError> {
    let candidate = new_user(email.clone(), name, None, false);
    let now = now_secs();
    let provider_owned = provider.to_string();
    let outcome = state
        .db
        .call(move |c| {
            if let Some(user) = db::user_by_identity(c, &provider_owned, &sub)? {
                return Ok(Ok(user));
            }
            let Some(email) = email else {
                return Ok(Err("provider did not supply an email address"));
            };
            if let Some(user) = db::user_by_email(c, &email)? {
                if !email_verified {
                    return Ok(Err(
                        "an account with this email exists but the provider email is not verified",
                    ));
                }
                db::link_identity(c, &provider_owned, &sub, &user.id, Some(&email))?;
                return Ok(Ok(user));
            }
            db::insert_user(c, &candidate, now)?;
            db::link_identity(c, &provider_owned, &sub, &candidate.id, Some(&email))?;
            crate::catalog::seed_default_library(c, &candidate.id, now)?;
            Ok(Ok(candidate))
        })
        .await?;
    let user = outcome.map_err(|msg| AppError::Status(StatusCode::CONFLICT, msg.into()))?;

    merge_guest_from_request(state, headers, &user.id).await?;

    let body = user_json(state, &user).await?;
    let mut resp = start_session(state, &user.id, StatusCode::FOUND, body).await?;
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_state_cookie(provider, state.config.cookie_secure()))
            .map_err(AppError::internal)?,
    );
    Ok(resp)
}
