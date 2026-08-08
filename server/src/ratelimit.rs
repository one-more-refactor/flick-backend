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
    pub friend_add: Rule,
    pub admin_login: Rule,
    pub delete_me: Rule,
    pub export: Rule,
    /// The MCP endpoint as a whole. Individual tools that cost real work
    /// (`import_url`) also charge their own bucket, so this is only the
    /// coarse "one client, one endpoint" ceiling.
    pub mcp: Rule,
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
            friend_add: Rule::new(30, FIVE_MIN),
            admin_login: Rule::new(10, FIVE_MIN),
            // Authed but destructive/expensive: a stolen cookie should not be
            // able to erase the account or pull the full export in a loop.
            delete_me: Rule::new(5, FIVE_MIN),
            export: Rule::new(10, HOUR),
            mcp: Rule::new(300, HOUR),
        }
    }
}

impl RateLimits {
    /// The rule (and its bucket name) for a request, or `None` = unlimited.
    /// Paths are the full ones — the middleware is layered OUTSIDE the `/api`
    /// nest, so the prefix is still present.
    fn rule_for(&self, method: &Method, path: &str) -> Option<(&'static str, Rule)> {
        match (method.as_str(), path) {
            ("POST", "/api/auth/login") => Some(("login", self.login)),
            ("POST", "/api/auth/register") => Some(("register", self.register)),
            ("POST", "/api/auth/code/request") => Some(("code_request", self.code_request)),
            ("POST", "/api/auth/code/verify") => Some(("code_verify", self.code_verify)),
            ("POST", "/api/auth/lookup") => Some(("lookup", self.lookup)),
            ("POST", "/api/auth/guest") => Some(("guest", self.guest)),
            ("POST", "/api/import/url") => Some(("import_url", self.import_url)),
            ("POST", "/api/friends/add") => Some(("friend_add", self.friend_add)),
            ("POST", "/api/admin/login") => Some(("admin_login", self.admin_login)),
            ("DELETE", "/api/auth/me") => Some(("delete_me", self.delete_me)),
            ("GET", "/api/auth/export") => Some(("export", self.export)),
            ("POST", "/mcp") => Some(("mcp", self.mcp)),
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

    /// Charge the shared outbound-fetch budget from outside the HTTP route.
    /// The MCP `import_url` tool spends the same allowance as
    /// `POST /api/import/url`, so an agent cannot use MCP to sidestep it.
    pub fn check_import_url(&self, client: &str) -> Result<(), u64> {
        self.check("import_url", self.limits.import_url, client)
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

/// Cloudflare's edge sets `CF-Connecting-IP` to the real client IP and strips
/// any client-supplied copy of it. The `cloudflared` tunnel worker connects
/// over loopback and does *not* set `X-Forwarded-For`, so in the tunnel
/// topology XFF is nothing but attacker-controlled bytes — prefer this header
/// and fall back to XFF for the bare-Caddy self-hosted topology.
fn cf_connecting_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("cf-connecting-ip")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The real client from X-Forwarded-For.
///
/// Read right-to-left: proxies *append*, so the rightmost entries are the ones
/// added by infrastructure we control and the leftmost is whatever the client
/// typed. Skip trusted proxies from the right and take the first address that
/// isn't one — that is the closest hop we did not add ourselves. Taking the
/// leftmost entry instead (as this did) let any client pick their own rate
/// limit bucket just by sending the header.
fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let ips: Vec<IpAddr> = headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .filter_map(|p| p.trim().parse().ok())
        .collect();

    ips.iter()
        .rev()
        .find(|ip| !trusted_proxy(**ip))
        .copied()
        // Every hop is one of ours: the rightmost is the best we can do.
        .or_else(|| ips.last().copied())
}

/// The client IP behind a peer we already decided to trust. Kept in one place
/// so the rate limiter and the signup-attribution path cannot drift apart.
fn client_behind_trusted_peer(headers: &HeaderMap) -> Option<IpAddr> {
    cf_connecting_ip(headers).or_else(|| forwarded_ip(headers))
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
                if let Some(client) = client_behind_trusted_peer(headers) {
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
        if let Some(client) = client_behind_trusted_peer(req.headers()) {
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
