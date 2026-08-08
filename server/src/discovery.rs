//! Agent-facing discovery surface.
//!
//! Everything an automated client needs to find its way around a flick server
//! without being told: `Link` headers on the HTML (RFC 8288), an API catalog
//! (RFC 9727/9264), an OpenAPI description, `/auth.md`, an agent-skills index,
//! an MCP server card, and markdown twins of the public HTML pages.
//!
//! All of it is embedded in the binary and rendered against the server's own
//! `FLICK_PUBLIC_URL`, so a self-hosted instance advertises itself correctly
//! with no extra configuration and no files to copy.

use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::AppPath;
use crate::AppState;

/// Placeholders substituted into every embedded document at serve time. The
/// version is one of them so the OpenAPI description can never claim a
/// release the binary is not.
const BASE: &str = "{{BASE}}";
const VERSION: &str = "{{VERSION}}";

const OPENAPI: &str = include_str!("../assets/agents/openapi.json");
const AUTH_MD: &str = include_str!("../assets/agents/auth.md");
const API_DOC: &str = include_str!("../assets/agents/api.md");
const INDEX_MD: &str = include_str!("../assets/agents/index.md");
const SCIENCE_MD: &str = include_str!("../assets/agents/science.md");

const MARKDOWN: &str = "text/markdown; charset=utf-8";
const LINKSET: &str = "application/linkset+json";

/// Rough token count for `x-markdown-tokens`: agents use it to decide whether
/// a document fits their budget before fetching it, so an estimate on the
/// generous side of the usual ~4-chars-per-token rule is the useful answer.
const CHARS_PER_TOKEN: usize = 4;

/// The skills this server publishes: (name, description, body). The
/// description is what an agent reads when deciding whether to load the skill,
/// so it must say when the skill applies, not just what it is about.
const SKILLS: &[(&str, &str, &str)] = &[
    (
        "flick-api",
        "Use the flick HTTP API to build and read a speed-reading library — mint an anonymous \
         session, import PDFs/EPUBs/URLs/text, search full-text, fetch paced RSVP timelines, and \
         track reading position and streaks. Use when working with flick, myflick.app, or any \
         self-hosted flick server.",
        include_str!("../assets/agents/skills/flick-api/SKILL.md"),
    ),
    (
        "rsvp-reading",
        "Advise honestly on RSVP speed reading — what it does and does not do, what wpm to \
         recommend for a given text, and how to build a reader that does not outrun \
         comprehension. Use when asked about speed reading, RSVP, words-per-minute claims, or \
         optimal recognition point (ORP) alignment.",
        include_str!("../assets/agents/skills/rsvp-reading/SKILL.md"),
    ),
    (
        "self-host-flick",
        "Deploy and operate a self-hosted flick speed-reading server — Compose or from source, \
         environment configuration, reverse proxy and cookie/rate-limit gotchas, OIDC/OAuth \
         single sign-on, SMTP, backups and upgrades. Use when installing, configuring, proxying \
         or troubleshooting a flick instance.",
        include_str!("../assets/agents/skills/self-host-flick/SKILL.md"),
    ),
];

/// Substitute the public base URL and the running version into an embedded
/// document.
fn render(doc: &str, base: &str) -> String {
    doc.replace(BASE, base)
        .replace(VERSION, env!("CARGO_PKG_VERSION"))
}

fn typed(content_type: &'static str, body: String) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// Markdown with the token estimate agents look for.
fn markdown(body: String) -> Response {
    let tokens = body.chars().count().div_ceil(CHARS_PER_TOKEN);
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(MARKDOWN)),
            (
                HeaderName::from_static("x-markdown-tokens"),
                HeaderValue::from_str(&tokens.to_string()).unwrap_or(HeaderValue::from_static("0")),
            ),
        ],
        body,
    )
        .into_response()
}

fn json_response(value: Value) -> Response {
    typed("application/json", value.to_string())
}

// ------------------------------------------------------------- documents

/// `/.well-known/api-catalog` (RFC 9727), serialized as a linkset (RFC 9264).
///
/// RFC 9264 names each relation as a *member* of the link context object,
/// which is what a conforming consumer reads. Several agent crawlers in the
/// wild instead expect a flat `links: [{rel, href}]` array, so we also emit
/// that — RFC 9264 tells consumers to ignore members they do not know, and
/// both views describe exactly the same three links.
async fn api_catalog(State(st): State<AppState>) -> Response {
    let base = &st.config.public_url;
    let desc = format!("{base}/openapi.json");
    let doc = format!("{base}/docs/api");
    let status = format!("{base}/api/meta");
    let auth = format!("{base}/auth.md");

    json_or_linkset(json!({
        "linkset": [{
            "anchor": format!("{base}/api"),
            "service-desc": [{ "href": desc, "type": "application/json" }],
            "service-doc": [{ "href": doc, "type": "text/markdown" }],
            "status": [{ "href": status, "type": "application/json" }],
            "authenticate": [{ "href": auth, "type": "text/markdown" }],
            "links": [
                { "rel": "service-desc", "href": desc, "type": "application/json" },
                { "rel": "service-doc", "href": doc, "type": "text/markdown" },
                { "rel": "status", "href": status, "type": "application/json" },
                { "rel": "authenticate", "href": auth, "type": "text/markdown" },
            ],
        }]
    }))
}

fn json_or_linkset(value: Value) -> Response {
    typed(LINKSET, value.to_string())
}

/// `/openapi.json` — the machine-readable API description.
async fn openapi(State(st): State<AppState>) -> Response {
    typed("application/json", render(OPENAPI, &st.config.public_url))
}

/// `/auth.md` — how an agent gets a credential.
async fn auth_md(State(st): State<AppState>) -> Response {
    markdown(render(AUTH_MD, &st.config.public_url))
}

/// `/docs/api` — the human-readable API documentation (`service-doc`).
async fn api_doc(State(st): State<AppState>) -> Response {
    markdown(render(API_DOC, &st.config.public_url))
}

/// `/.well-known/mcp/server-card.json` (SEP-1649) — points at the MCP
/// endpoint this same server runs.
async fn mcp_server_card(State(st): State<AppState>) -> Response {
    let base = &st.config.public_url;
    json_response(json!({
        "serverInfo": {
            "name": "flick",
            "title": "flick — speed reading",
            "version": env!("CARGO_PKG_VERSION"),
            "websiteUrl": base,
        },
        "description": "Paced RSVP reading: turn text into a word-by-word timeline with \
                        optimal-recognition-point alignment, browse the public-domain catalog, \
                        and search or import into a reading library.",
        "protocolVersion": crate::mcp::PROTOCOL_VERSION,
        "transport": {
            "type": "streamable-http",
            "endpoint": format!("{base}/mcp"),
        },
        "capabilities": { "tools": true, "resources": false, "prompts": false },
        "authentication": {
            "required": false,
            "description": "Public tools need no credential. Library tools use the flick_session \
                            cookie; see /auth.md.",
            "documentation": format!("{base}/auth.md"),
        },
        "documentation": format!("{base}/docs/api"),
    }))
}

/// SHA-256 of the bytes we actually serve, in the `sha256:{hex}` form the
/// discovery RFC specifies.
fn digest(body: &str) -> String {
    let hash = Sha256::digest(body.as_bytes());
    let mut out = String::with_capacity(7 + hash.len() * 2);
    out.push_str("sha256:");
    for byte in hash {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `/.well-known/agent-skills/index.json` (Agent Skills Discovery RFC v0.2.0).
async fn agent_skills_index(State(st): State<AppState>) -> Response {
    let base = &st.config.public_url;
    let skills: Vec<Value> = SKILLS
        .iter()
        .map(|(name, description, body)| {
            json!({
                "name": name,
                "type": "skill-md",
                "description": description,
                "url": format!("{base}/.well-known/agent-skills/{name}/SKILL.md"),
                "digest": digest(&render(body, base)),
                "license": "AGPL-3.0-only",
            })
        })
        .collect();

    json_response(json!({
        "$schema": "https://schemas.agentskills.io/discovery/0.2.0/schema.json",
        "version": "0.2.0",
        "skills": skills,
    }))
}

/// `/.well-known/agent-skills/{name}/SKILL.md`
async fn agent_skill(State(st): State<AppState>, AppPath(name): AppPath<String>) -> Response {
    match SKILLS.iter().find(|(n, _, _)| *n == name) {
        Some((_, _, body)) => markdown(render(body, &st.config.public_url)),
        None => not_found(),
    }
}

/// Anything else under `/.well-known/` is a 404 *as JSON*. Without this the
/// SPA fallback answers every probe with 200 + HTML, which reads to a scanner
/// as "this document exists and is malformed" rather than "not published".
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "error": "not found" }).to_string(),
    )
        .into_response()
}

async fn well_known_fallback() -> Response {
    not_found()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/openapi.json", get(openapi))
        .route("/auth.md", get(auth_md))
        .route("/docs/api", get(api_doc))
        .route("/.well-known/api-catalog", get(api_catalog))
        .route("/.well-known/mcp/server-card.json", get(mcp_server_card))
        .route(
            "/.well-known/agent-skills/index.json",
            get(agent_skills_index),
        )
        .route(
            "/.well-known/agent-skills/{name}/SKILL.md",
            get(agent_skill),
        )
        .route("/.well-known/{*rest}", get(well_known_fallback))
}

// ------------------------------------------------------- content negotiation

/// HTML pages that have an agent-facing markdown twin.
fn markdown_twin(path: &str) -> Option<&'static str> {
    match path.trim_end_matches('/') {
        "" => Some(INDEX_MD),
        "/science" => Some(SCIENCE_MD),
        _ => None,
    }
}

/// Whether the client asked for markdown and did not merely tolerate it.
///
/// `Accept: */*` — what every default HTTP client sends — must NOT count, or
/// browsers and curl would start getting markdown instead of the site.
fn wants_markdown(accept: Option<&str>) -> bool {
    let Some(accept) = accept else { return false };
    accept.split(',').any(|part| {
        let mut params = part.split(';').map(str::trim);
        let media = params.next().unwrap_or("");
        if !media.eq_ignore_ascii_case("text/markdown") {
            return false;
        }
        // q=0 is an explicit refusal.
        !params.any(|p| {
            p.strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q <= 0.0)
        })
    })
}

/// Serve markdown to agents that ask for it, HTML to everyone else, from the
/// same URL (Cloudflare's "Markdown for Agents" shape).
pub async fn markdown_negotiation(
    State(st): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let negotiable = matches!(*req.method(), Method::GET | Method::HEAD)
        && markdown_twin(req.uri().path()).is_some();

    if negotiable {
        let accept = req
            .headers()
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok());
        if wants_markdown(accept) {
            let doc = markdown_twin(req.uri().path()).expect("checked above");
            let mut res = markdown(render(doc, &st.config.public_url));
            res.headers_mut()
                .append(header::VARY, HeaderValue::from_static("accept"));
            return res;
        }
    }

    let mut res = next.run(req).await;
    if negotiable {
        // Tell caches the HTML at this URL is only one of two representations.
        res.headers_mut()
            .append(header::VARY, HeaderValue::from_static("accept"));
    }
    res
}

// ------------------------------------------------------------ link headers

/// `Link:` on every HTML page (RFC 8288) so an agent that fetches nothing but
/// the homepage still finds the API description, the docs, the catalog, the
/// MCP card and the licence.
pub async fn link_headers(
    State(st): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let mut res = next.run(req).await;

    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if !is_html || !res.status().is_success() {
        return res;
    }

    let base = &st.config.public_url;
    let value = format!(
        "<{base}/.well-known/api-catalog>; rel=\"api-catalog\"; type=\"application/linkset+json\", \
         <{base}/openapi.json>; rel=\"service-desc\"; type=\"application/json\", \
         <{base}/docs/api>; rel=\"service-doc\"; type=\"text/markdown\", \
         <{base}/.well-known/mcp/server-card.json>; rel=\"service-meta\"; type=\"application/json\", \
         <{base}/llms.txt>; rel=\"describedby\"; type=\"text/plain\", \
         <{base}/.well-known/agent-skills/index.json>; rel=\"describedby\"; type=\"application/json\", \
         <https://www.gnu.org/licenses/agpl-3.0.html>; rel=\"license\""
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        res.headers_mut().append(header::LINK, value);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_markdown_wins() {
        assert!(wants_markdown(Some("text/markdown")));
        assert!(wants_markdown(Some("text/markdown;q=0.9, text/html;q=0.8")));
        assert!(wants_markdown(Some("text/html, text/markdown")));
        // The defaults every browser and curl send must keep getting HTML.
        assert!(!wants_markdown(Some("*/*")));
        assert!(!wants_markdown(Some(
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"
        )));
        assert!(!wants_markdown(Some("text/markdown;q=0")));
        assert!(!wants_markdown(None));
    }

    #[test]
    fn twins_ignore_trailing_slash() {
        assert!(markdown_twin("/").is_some());
        assert!(markdown_twin("/science").is_some());
        assert!(markdown_twin("/science/").is_some());
        assert!(markdown_twin("/library").is_none());
    }

    #[test]
    fn digests_are_hex_sha256() {
        let d = digest("flick");
        assert!(d.starts_with("sha256:"));
        assert_eq!(d.len(), 7 + 64);
        assert!(d[7..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn embedded_documents_are_rendered_against_the_public_url() {
        for (_, _, body) in SKILLS {
            let rendered = render(body, "https://example.test");
            assert!(!rendered.contains(BASE), "unrendered placeholder in skill");
            assert!(!rendered.contains(VERSION), "unrendered version in skill");
        }
        for doc in [OPENAPI, AUTH_MD, API_DOC, INDEX_MD, SCIENCE_MD] {
            let rendered = render(doc, "https://example.test");
            assert!(!rendered.contains(BASE), "unrendered placeholder in doc");
            assert!(!rendered.contains(VERSION), "unrendered version in doc");
        }
    }

    #[test]
    fn openapi_is_valid_json_after_rendering() {
        let rendered = render(OPENAPI, "https://example.test");
        let parsed: Value = serde_json::from_str(&rendered).expect("openapi.json parses");
        assert_eq!(parsed["openapi"], "3.1.0");
        assert_eq!(parsed["info"]["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn every_skill_declares_its_own_name() {
        for (name, _, body) in SKILLS {
            let front = body.lines().nth(1).unwrap_or_default();
            assert_eq!(front, format!("name: {name}"), "frontmatter name mismatch");
        }
    }
}
