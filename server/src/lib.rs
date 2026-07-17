//! flick-server: axum backend for the flick speed-reading app.
//!
//! API surface, timeline format, env config, and error shape are specified in
//! docs/CONTRACTS.md. Parsing lives exclusively in flick-core.

pub mod auth;
pub mod books;
pub mod config;
pub mod db;
pub mod error;
pub mod oidc;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::Router;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::Level;

use crate::config::Config;
use crate::error::AppError;

#[derive(Clone)]
pub struct AppState {
    pub db: db::Db,
    pub config: Arc<Config>,
    pub oidc: Arc<oidc::OidcRuntime>,
}

impl AppState {
    pub fn new(db: db::Db, config: Config) -> Self {
        AppState {
            db,
            config: Arc::new(config),
            oidc: Arc::new(oidc::OidcRuntime::default()),
        }
    }
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
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me).patch(auth::update_me))
        .route("/auth/providers", get(auth::providers))
        .route("/auth/oidc/login", get(oidc::login))
        .route("/auth/oidc/callback", get(oidc::callback))
        .route("/books", get(books::list).post(books::create))
        .route(
            "/books/{id}",
            get(books::get_book).delete(books::delete_book),
        )
        .route("/books/{id}/timeline", get(books::timeline))
        .route("/books/{id}/position", put(books::set_position))
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
        .layer(middleware::from_fn(cache_control))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}
