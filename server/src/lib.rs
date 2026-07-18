//! flick-server: axum backend for the flick speed-reading app.
//!
//! API surface, timeline format, env config, and error shape are specified in
//! docs/CONTRACTS.md. Parsing lives exclusively in flick-core.

pub mod auth;
pub mod books;
pub mod catalog;
pub mod config;
pub mod db;
pub mod error;
pub mod import;
pub mod integrations;
pub mod mail;
pub mod oidc;
pub mod ratelimit;
pub mod referral;
pub mod social;
pub mod stats;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Config;
use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    pub db: db::Db,
    pub config: Arc<Config>,
    pub oauth: Arc<oidc::OauthRuntime>,
    pub limiter: Arc<ratelimit::RateLimiter>,
}

impl AppState {
    pub fn new(db: db::Db, config: Config) -> Self {
        AppState {
            db,
            config: Arc::new(config),
            oauth: Arc::new(oidc::OauthRuntime::default()),
            limiter: Arc::new(ratelimit::RateLimiter::new(ratelimit::RateLimits::default())),
        }
    }

    /// Replace the contract-default rate limits (tests use tiny ones).
    pub fn with_rate_limits(mut self, limits: ratelimit::RateLimits) -> Self {
        self.limiter = Arc::new(ratelimit::RateLimiter::new(limits));
        self
    }
}

/// GET /api/meta — public, no auth: which edition this server runs and its
/// version. Clients switch the Pro/Contribute UI on `edition` (CONTRACTS.md
/// "Editions & plans").
async fn meta(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "edition": state.config.edition.as_str(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn api_not_found() -> AppError {
    AppError::NotFound
}

async fn api_method_not_allowed() -> AppError {
    AppError::Status(
        StatusCode::METHOD_NOT_ALLOWED,
        "method not allowed".into(),
    )
}

/// Cache policy for the static web client: hashed assets are immutable,
/// everything else (index.html, manifest, sw.js) must revalidate — otherwise
/// browsers heuristically cache the shell and users see stale deploys.
/// `/api` responses are left untouched.
async fn cache_control(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    if !path.starts_with("/api") && res.status().is_success() {
        let value = if path.starts_with("/assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    }
    res
}

/// Plain-text fallback when FLICK_WEB_DIST has no built web client.
async fn no_web_dist(uri: Uri) -> Response {
    let status = if uri.path() == "/" {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    (
        status,
        "flick-server: web dist not found — build it (cd web && bun run build), \
         run from the repo root, or set FLICK_WEB_DIST",
    )
        .into_response()
}

fn api_router() -> Router<AppState> {
    Router::new()
        .route("/meta", get(meta))
        .route("/auth/guest", post(auth::guest))
        .route("/auth/lookup", post(auth::lookup))
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/code/request", post(auth::code_request))
        .route("/auth/code/verify", post(auth::code_verify))
        .route("/auth/logout", post(auth::logout))
        .route(
            "/auth/me",
            get(auth::me).patch(auth::update_me).delete(auth::delete_me),
        )
        .route("/auth/export", get(auth::export_me))
        .route("/auth/providers", get(auth::providers))
        .route("/auth/oauth/{provider}/login", get(oidc::login))
        .route("/auth/oauth/{provider}/callback", get(oidc::callback))
        .route("/auth/oidc/login", get(oidc::oidc_login_alias))
        .route("/auth/oidc/callback", get(oidc::oidc_callback_alias))
        .route("/books", get(books::list).post(books::create))
        .route("/books/trash", get(books::list_trash))
        .route("/books/{id}/restore", post(books::restore_book))
        .route("/books/{id}/purge", delete(books::purge_book))
        .route("/books/{id}/tags", put(books::set_tags))
        .route(
            "/books/{id}/share",
            post(books::share_book).delete(books::unshare_book),
        )
        .route("/shared/{token}", get(books::shared_info))
        .route("/shared/{token}/timeline", get(books::shared_timeline))
        .route("/shared/{token}/import", post(books::shared_import))
        .route("/import/url", post(books::import_url))
        .route("/import/html", post(books::import_html))
        .route(
            "/books/{id}",
            get(books::get_book).delete(books::delete_book),
        )
        .route("/books/{id}/timeline", get(books::timeline))
        .route("/books/{id}/text", get(books::text))
        .route("/books/{id}/position", put(books::set_position))
        .route("/integrations", get(integrations::integrations))
        .route("/stats", get(stats::stats))
        .route(
            "/sessions",
            get(stats::list_sessions).post(stats::create_session),
        )
        .route("/referral", get(referral::status))
        .route("/events/active", get(referral::active))
        .route("/admin/events", get(referral::admin_list).post(referral::admin_create))
        .route("/admin/events/{id}", delete(referral::admin_delete))
        .route("/friends", get(social::list))
        .route("/friends/link", get(social::link))
        .route("/friends/add", post(social::add))
        .route("/friends/{id}", delete(social::remove))
        .route("/wrapped", get(social::wrapped))
        .route("/catalog", get(catalog::list))
        .route("/catalog/{slug}/add", post(catalog::add))
        .fallback(api_not_found)
        .method_not_allowed_fallback(api_method_not_allowed)
        .layer(DefaultBodyLimit::max(books::UPLOAD_LIMIT))
}

/// Build the full application router (API + static web client with SPA
/// fallback, or a plain-text notice when the dist dir is missing).
pub fn app(state: AppState) -> Router {
    let router = Router::new().nest("/api", api_router());

    let index = state.config.web_dist.join("index.html");
    let router = if index.is_file() {
        router.fallback_service(
            ServeDir::new(&state.config.web_dist).fallback(ServeFile::new(index)),
        )
    } else {
        router.fallback(no_web_dist)
    };

    router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::rate_limit,
        ))
        .layer(middleware::from_fn(cache_control))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}
