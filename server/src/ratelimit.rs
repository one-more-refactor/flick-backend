//! In-memory per-client rate limiting for the abuse-prone endpoints
//! (docs/CONTRACTS.md "Rate limits").
//!
//! Fixed-window counters: the first request from a (endpoint, client) pair
//! opens a window; up to `max` requests pass until the window expires, then
//! the counter resets. Simple and memory-light; the known trade-off is a
//! worst-case 2× burst across a window boundary, which is fine for abuse
//! resistance (this is not traffic shaping). State is a plain
//! `Mutex<HashMap>` swept lazily — no background task, no extra deps, and
//! per-process only (each replica counts independently).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::AppState;

const FIVE_MIN: Duration = Duration::from_secs(5 * 60);
const HOUR: Duration = Duration::from_secs(60 * 60);

/// Expired windows are dropped at most this often (piggybacked on `check`,
/// under the same lock) so the map cannot grow without bound.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// One limit: at most `max` requests per fixed `window`.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub max: u32,
    pub window: Duration,
}

impl Rule {
    pub const fn new(max: u32, window: Duration) -> Self {
        Rule { max, window }
    }
}

/// The per-endpoint limits table. `Default` is the contract; tests construct
/// smaller ones via `AppState::with_rate_limits`.
#[derive(Debug, Clone, Copy)]
pub struct RateLimits {
    pub login: Rule,
    pub register: Rule,
    pub code_request: Rule,
    pub code_verify: Rule,
    pub lookup: Rule,
    pub guest: Rule,
    pub import_url: Rule,
}

impl Default for RateLimits {
    fn default() -> Self {
        RateLimits {
            login: Rule::new(10, FIVE_MIN),
            register: Rule::new(10, FIVE_MIN),
            code_request: Rule::new(5, FIVE_MIN),
            code_verify: Rule::new(10, FIVE_MIN),
            lookup: Rule::new(30, FIVE_MIN),
            guest: Rule::new(20, HOUR),
            import_url: Rule::new(30, HOUR),
        }
    }
}

impl RateLimits {
    /// The rule (and its bucket name) for a request, or `None` = unlimited.
    /// Paths are the full ones — the middleware is layered OUTSIDE the `/api`
    /// nest, so the prefix is still present.
    fn rule_for(&self, method: &Method, path: &str) -> Option<(&'static str, Rule)> {
        if *method != Method::POST {
            return None;
        }
        match path {
            "/api/auth/login" => Some(("login", self.login)),
            "/api/auth/register" => Some(("register", self.register)),
            "/api/auth/code/request" => Some(("code_request", self.code_request)),
            "/api/auth/code/verify" => Some(("code_verify", self.code_verify)),
            "/api/auth/lookup" => Some(("lookup", self.lookup)),
            "/api/auth/guest" => Some(("guest", self.guest)),
            "/api/import/url" => Some(("import_url", self.import_url)),
            _ => None,
        }
    }
}

struct Window {
    reset_at: Instant,
    count: u32,
}

struct Buckets {
    map: HashMap<(&'static str, String), Window>,
    next_sweep: Instant,
}

pub struct RateLimiter {
    limits: RateLimits,
    buckets: Mutex<Buckets>,
}

impl RateLimiter {
    pub fn new(limits: RateLimits) -> Self {
        RateLimiter {
            limits,
            buckets: Mutex::new(Buckets {
                map: HashMap::new(),
                next_sweep: Instant::now() + SWEEP_EVERY,
            }),
        }
    }

    /// Count one request against `(endpoint, client)`. `Ok` = allowed;
    /// `Err(secs)` = over the limit, retry after that many seconds.
    fn check(&self, endpoint: &'static str, rule: Rule, client: &str) -> Result<(), u64> {
        let now = Instant::now();
        // A poisoned lock only means a panic mid-bookkeeping; the counters
        // are still coherent enough for limiting, so keep going.
        let mut buckets = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if now >= buckets.next_sweep {
            buckets.map.retain(|_, w| w.reset_at > now);
            buckets.next_sweep = now + SWEEP_EVERY;
        }
        let window = buckets
            .map
            .entry((endpoint, client.to_string()))
            .or_insert(Window {
                reset_at: now + rule.window,
                count: 0,
            });
        if now >= window.reset_at {
            *window = Window {
                reset_at: now + rule.window,
                count: 0,
            };
        }
        if window.count >= rule.max {
            let left = window.reset_at - now;
            // Ceil to whole seconds so "Retry-After: 0" never happens.
            return Err((left.as_secs() + u64::from(left.subsec_nanos() > 0)).max(1));
        }
        window.count += 1;
        Ok(())
    }
}

// ----------------------------------------------------------- client key

/// A peer we trust to have set X-Forwarded-For honestly: loopback, RFC1918,
/// CGNAT/tailnet 100.64/10, or ULA fc00::/7 — i.e. the reverse proxy (Caddy)
/// on this box or LAN. Anything public speaks for itself.
fn trusted_proxy(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// The FIRST entry of the first X-Forwarded-For header, if it parses as an
/// IP. Caddy overwrites/appends the header itself, so with exactly one proxy
/// in front the first entry is the real client.
fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Infallible extractor: the request's client IP under the same trust rules
/// as the rate limiter ("unknown" without ConnectInfo, e.g. in tests).
pub struct ClientIp(pub String);

impl<S> axum::extract::FromRequestParts<S> for ClientIp
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        Ok(ClientIp(client_ip(&parts.headers, peer)))
    }
}

/// Client IP from headers + optional peer (signup attribution, referral
/// anti-abuse). Same trust rules as the rate limiter.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    match peer {
        None => "unknown".into(),
        Some(peer) => {
            let peer_ip = peer.ip().to_canonical();
            if trusted_proxy(peer_ip) {
                if let Some(client) = forwarded_ip(headers) {
                    return client.to_canonical().to_string();
                }
            }
            peer_ip.to_string()
        }
    }
}

/// The rate-limit key for a request: the forwarded client IP when the direct
/// peer is a trusted proxy, else the peer IP itself. Without `ConnectInfo`
/// (router driven directly, e.g. `oneshot` in tests) the key is `"unknown"`
/// — never a panic.
fn client_key(req: &Request) -> String {
    let Some(ConnectInfo(peer)) = req.extensions().get::<ConnectInfo<SocketAddr>>() else {
        return "unknown".into();
    };
    // Canonicalize so an IPv4-mapped ::ffff:a.b.c.d peer matches the v4 rules.
    let peer_ip = peer.ip().to_canonical();
    if trusted_proxy(peer_ip) {
        if let Some(client) = forwarded_ip(req.headers()) {
            return client.to_canonical().to_string();
        }
    }
    peer_ip.to_string()
}

// ----------------------------------------------------------- middleware

/// Middleware over the whole app: unlimited routes pass straight through;
/// limited ones 429 with the standard error shape + Retry-After (seconds).
pub async fn rate_limit(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some((endpoint, rule)) = state
        .limiter
        .limits
        .rule_for(req.method(), req.uri().path())
    else {
        return next.run(req).await;
    };
    let key = client_key(&req);
    match state.limiter.check(endpoint, rule, &key) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => {
            tracing::warn!(%endpoint, %key, retry_after, "rate limited");
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_after.to_string())],
                Json(json!({"error": "too many requests — try again later"})),
            )
                .into_response()
        }
    }
}
