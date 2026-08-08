//! flick-server: axum backend for the flick speed-reading app.
//!
//! API surface, timeline format, env config, and error shape are specified in
//! docs/CONTRACTS.md. Parsing lives exclusively in flick-core.

pub mod admin;
pub mod auth;
pub mod books;
pub mod catalog;
pub mod config;
pub mod db;
pub mod discovery;
pub mod error;
pub mod import;
pub mod integrations;
pub mod mail;
pub mod mcp;
pub mod oidc;
pub mod ratelimit;
pub mod referral;
pub mod social;
pub mod stats;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
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
    /// Process start, for /api/admin/overview uptime.
    pub started: std::time::Instant,
}

impl AppState {
    pub fn new(db: db::Db, config: Config) -> Self {
        AppState {
            db,
            config: Arc::new(config),
            oauth: Arc::new(oidc::OauthRuntime::default()),
            limiter: Arc::new(ratelimit::RateLimiter::new(ratelimit::RateLimits::default())),
            started: std::time::Instant::now(),
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
async fn meta(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let announcement = state.db.call(db::announcement_public).await?;
    Ok(Json(json!({
        "edition": state.config.edition.as_str(),
        "version": env!("CARGO_PKG_VERSION"),
        "announcement": announcement,
        "admin_url": state.config.admin_url,
    })))
}

async fn api_not_found() -> AppError {
    AppError::NotFound
}

async fn api_method_not_allowed() -> AppError {
    AppError::Status(StatusCode::METHOD_NOT_ALLOWED, "method not allowed".into())
}

/// Cache policy for the static web client: hashed assets are immutable,
/// everything else (index.html, manifest, sw.js) must revalidate — otherwise
/// browsers heuristically cache the shell and users see stale deploys.
/// `/api` responses are left untouched.
async fn cache_control(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut res = next.run(req).await;
    if !path.starts_with("/api") && res.status().is_success() {
        // /assets/ is the pre-1.1 single-SPA layout; /app/assets/ is the SPA
        // under the combined layout; /_astro/ is the landing's hashed output.
        let hashed = path.starts_with("/assets/")
            || path.starts_with("/app/assets/")
            || path.starts_with("/_astro/");
        let value = if hashed {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    }
    res
}

/// Security headers on every response, API and static alike.
///
/// The Cloudflare edge can add these too, but setting them at the origin keeps
/// one source of truth and means a bare self-hosted deployment behind plain
/// Caddy gets them as well.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // One year, but no `includeSubDomains`: the apex and the app/admin
    // subdomains are not all guaranteed to be HTTPS-only forever, and this
    // header is very hard to walk back once a browser has pinned it.
    h.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static("max-age=31536000"),
    );
    res
}

/// Pre-1.1 the SPA owned the site root; its deep links live under /app/ now.
/// Permanent redirects keep every shared/bookmarked URL working.
async fn legacy_app_path(uri: Uri) -> Redirect {
    Redirect::permanent(&format!("/app{}", uri.path()))
}

/// The 1.1 era briefly served the SPA under /app/; 1.2 puts it back at the
/// root. Links minted in that window redirect home with the prefix stripped.
async fn strip_app_prefix(uri: Uri) -> Redirect {
    let path = uri.path().strip_prefix("/app").unwrap_or("/");
    let path = if path.is_empty() { "/" } else { path };
    Redirect::permanent(path)
}

/// 308 any request arriving on a retired hostname (FLICK_LEGACY_HOSTS, e.g.
/// app.myflick.app) to the same path on the canonical public URL. Share links
/// and bookmarks from the subdomain era keep working forever.
async fn legacy_host_redirect(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if !st.config.legacy_hosts.is_empty() {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h).to_ascii_lowercase());
        if let Some(host) = host {
            if st.config.legacy_hosts.contains(&host) {
                let pq = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or("/");
                return Redirect::permanent(&format!("{}{}", st.config.public_url, pq))
                    .into_response();
            }
        }
    }
    next.run(req).await
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
        .route(
            "/admin/events",
            get(referral::admin_list).post(referral::admin_create),
        )
        .route("/admin/events/{id}", delete(referral::admin_delete))
        .route("/admin/login", post(admin::login))
        .route("/admin/session", delete(admin::logout))
        .route("/admin/me", get(admin::me))
        .route("/admin/overview", get(admin::overview))
        .route("/admin/users", get(admin::users_list))
        .route(
            "/admin/users/{id}",
            patch(admin::user_patch).delete(admin::user_delete),
        )
        .route(
            "/admin/announcement",
            get(admin::announcement_get).put(admin::announcement_put),
        )
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
    // The agent surface is merged before the static fallback so that
    // `/.well-known/*`, `/openapi.json` and `/auth.md` answer with real
    // documents and real 404s instead of the SPA shell.
    let router = Router::new()
        .nest("/api", api_router())
        .merge(mcp::routes())
        .merge(discovery::routes());

    let index = state.config.web_dist.join("index.html");
    let app_index = state.config.web_dist.join("app").join("index.html");
    let router = if app_index.is_file() {
        // Combined layout (web 1.1+): the static landing owns the root, the
        // SPA lives under /app/ with its own SPA fallback, and the old
        // root-level deep links 301 into /app/ so shared URLs keep working.
        router
            .nest_service(
                "/app",
                ServeDir::new(state.config.web_dist.join("app"))
                    .fallback(ServeFile::new(app_index)),
            )
            .route("/read/{id}", get(legacy_app_path))
            .route("/s/{token}", get(legacy_app_path))
            .route("/r/{code}", get(legacy_app_path))
            .route("/f/{code}", get(legacy_app_path))
            .route("/stats", get(legacy_app_path))
            .route("/auth", get(legacy_app_path))
            .route("/premium", get(legacy_app_path))
            .route("/invite", get(legacy_app_path))
            .route("/wrapped", get(legacy_app_path))
            .fallback_service(ServeDir::new(&state.config.web_dist).fallback(ServeFile::new(index)))
    } else if index.is_file() {
        // Single-SPA layout (the 1.2+ and pre-1.1 shape): SPA at the root;
        // /app/* links from the brief 1.1 split era strip back home.
        router
            .route("/app", get(strip_app_prefix))
            .route("/app/", get(strip_app_prefix))
            .route("/app/{*rest}", get(strip_app_prefix))
            .fallback_service(ServeDir::new(&state.config.web_dist).fallback(ServeFile::new(index)))
    } else {
        router.fallback(no_web_dist)
    };

    // The admin panel lives on its own origin (CONTRACTS.md "Admin API &
    // panel"); auth is bearer-only, so no credentialed CORS is ever needed.
    let router = match state.config.admin_origin.as_deref() {
        Some(origin) => match origin.parse::<HeaderValue>() {
            Ok(origin) => router.layer(
                CorsLayer::new()
                    .allow_origin(origin)
                    .allow_methods([
                        axum::http::Method::GET,
                        axum::http::Method::POST,
                        axum::http::Method::PUT,
                        axum::http::Method::PATCH,
                        axum::http::Method::DELETE,
                    ])
                    .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
            ),
            Err(_) => {
                tracing::warn!("FLICK_ADMIN_ORIGIN is not a valid origin; CORS disabled");
                router
            }
        },
        None => router,
    };

    router
        // Innermost, so the markdown it substitutes for HTML is compressed and
        // gets the same cache and security headers as everything else.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            discovery::markdown_negotiation,
        ))
        .layer(CompressionLayer::new())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            legacy_host_redirect,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            ratelimit::rate_limit,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            discovery::link_headers,
        ))
        .layer(middleware::from_fn(cache_control))
        .layer(middleware::from_fn(security_headers))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .with_state(state)
}
