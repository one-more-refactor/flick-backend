//! Optional generic OIDC SSO (Authentik-first).
//!
//! Enabled only when FLICK_OIDC_ISSUER + client id/secret are all set.
//! Provider discovery is lazy (first use) so the server starts fine when the
//! issuer is unreachable; discovery failures surface as 502s on the OIDC
//! routes only.

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
use tokio::sync::OnceCell;

use crate::auth::{random_token, start_session, user_json};
use crate::config::Config;
use crate::db::{self, now_secs, User};
use crate::error::AppError;
use crate::AppState;

const STATE_COOKIE: &str = "flick_oidc";
const STATE_TTL_SECS: i64 = 600; // 10 minutes

type Client = CoreClient<
    EndpointSet,      // auth url (from discovery)
    EndpointNotSet,   // device auth
    EndpointNotSet,   // introspection
    EndpointNotSet,   // revocation
    EndpointMaybeSet, // token url
    EndpointMaybeSet, // userinfo url
>;

/// Lazily-discovered OIDC client, shared across requests.
#[derive(Default)]
pub struct OidcRuntime {
    cell: OnceCell<(Client, openidconnect::reqwest::Client)>,
}

impl OidcRuntime {
    async fn client(
        &self,
        config: &Config,
    ) -> Result<&(Client, openidconnect::reqwest::Client), AppError> {
        let settings = config.oidc.as_ref().ok_or(AppError::NotFound)?;
        self.cell
            .get_or_try_init(|| async {
                let http = openidconnect::reqwest::ClientBuilder::new()
                    // Required by openidconnect: never follow redirects.
                    .redirect(openidconnect::reqwest::redirect::Policy::none())
                    .build()
                    .map_err(AppError::internal)?;
                let issuer = IssuerUrl::new(settings.issuer.clone())
                    .map_err(|e| AppError::Internal(format!("bad FLICK_OIDC_ISSUER: {e}")))?;
                let metadata = CoreProviderMetadata::discover_async(issuer, &http)
                    .await
                    .map_err(|e| {
                        AppError::Status(
                            StatusCode::BAD_GATEWAY,
                            format!("OIDC provider discovery failed: {e}"),
                        )
                    })?;
                let redirect = RedirectUrl::new(format!(
                    "{}/api/auth/oidc/callback",
                    config.public_url
                ))
                .map_err(AppError::internal)?;
                let client = CoreClient::from_provider_metadata(
                    metadata,
                    ClientId::new(settings.client_id.clone()),
                    Some(ClientSecret::new(settings.client_secret.clone())),
                )
                .set_redirect_uri(redirect);
                Ok((client, http))
            })
            .await
    }
}

fn state_cookie(value: &str, secure: bool) -> String {
    format!(
        "{STATE_COOKIE}={value}; Path=/api/auth/oidc; HttpOnly; SameSite=Lax; Max-Age={STATE_TTL_SECS}{}",
        if secure { "; Secure" } else { "" }
    )
}

fn clear_state_cookie(secure: bool) -> String {
    format!(
        "{STATE_COOKIE}=; Path=/api/auth/oidc; HttpOnly; SameSite=Lax; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

/// GET /api/auth/oidc/login — 302 to the IdP (404 when OIDC unconfigured).
pub async fn login(State(state): State<AppState>) -> Result<Response, AppError> {
    let (client, _) = state.oidc.client(&state.config).await?;

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
    let cookie = state_cookie(&payload, state.config.cookie_secure());
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, auth_url.as_str())
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .map_err(AppError::internal)
}

/// GET /api/auth/oidc/callback — code exchange, create-or-link user, 302 /.
pub async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> Result<Response, AppError> {
    let (client, http) = state.oidc.client(&state.config).await?;

    let query: std::collections::HashMap<String, String> = Query::try_from_uri(&uri)
        .map(|Query(q)| q)
        .map_err(|e| AppError::bad_request(e.body_text()))?;

    if let Some(err) = query.get("error") {
        let desc = query
            .get("error_description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        return Err(AppError::bad_request(format!("OIDC error: {err}{desc}")));
    }
    let code = query
        .get("code")
        .ok_or_else(|| AppError::bad_request("missing OIDC code parameter"))?;
    let returned_state = query
        .get("state")
        .ok_or_else(|| AppError::bad_request("missing OIDC state parameter"))?;

    let cookie = crate::auth::cookie_value(&headers, STATE_COOKIE)
        .ok_or_else(|| AppError::bad_request("missing or expired OIDC state cookie"))?;
    let mut parts = cookie.splitn(3, '|');
    let (Some(saved_state), Some(nonce), Some(verifier)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err(AppError::bad_request("malformed OIDC state cookie"));
    };
    if saved_state != returned_state {
        return Err(AppError::bad_request("OIDC state mismatch"));
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

    // Match by sub; else link a verified-email account; else create.
    let new_id = random_token(16);
    let now = now_secs();
    let outcome = state
        .db
        .call(move |c| {
            if let Some(user) = db::user_by_oidc_sub(c, &sub)? {
                return Ok(Ok(user));
            }
            let Some(email) = email else {
                return Ok(Err("OIDC provider did not supply an email address"));
            };
            if let Some(user) = db::user_by_email(c, &email)? {
                if !email_verified {
                    return Ok(Err(
                        "an account with this email exists but the OIDC email is not verified",
                    ));
                }
                db::link_oidc_sub(c, &user.id, &sub)?;
                return Ok(Ok(user));
            }
            let user = User {
                id: new_id,
                email,
                name,
                password_hash: None,
            };
            db::insert_user(c, &user, Some(&sub), now)?;
            Ok(Ok(user))
        })
        .await?;
    let user = outcome.map_err(|msg| AppError::Status(StatusCode::CONFLICT, msg.into()))?;

    let mut resp = start_session(&state, &user.id, StatusCode::FOUND, user_json(&user)).await?;
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static("/"));
    resp.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_state_cookie(state.config.cookie_secure()))
            .map_err(AppError::internal)?,
    );
    Ok(resp)
}
