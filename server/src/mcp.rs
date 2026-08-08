//! Model Context Protocol endpoint (`POST /mcp`, streamable HTTP transport).
//!
//! Every tool here is a thin front for logic that already exists elsewhere in
//! this server — the same reading engine, the same catalog, the same
//! SSRF-guarded importer, the same per-user scoping. Nothing is reimplemented
//! for agents, so an agent and the web client cannot drift apart.
//!
//! Auth follows the rest of the server: the `flick_session` cookie. Tools that
//! touch a library need one; the engine and catalog tools do not. The endpoint
//! is deliberately **not** in the CORS allow-list, and the session cookie is
//! `SameSite=Lax`, so a hostile page cannot drive a signed-in user's library
//! from their browser.

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use flick_core::Timeline;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::config::Edition;
use crate::db::{self, User};
use crate::error::AppError;
use crate::{books, catalog, stats, AppState};

/// The MCP revision this endpoint implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Upper bound on text handed to `preview_timeline`. Books go through the
/// library endpoints, which have their own 25 MB cap; this tool is for
/// pasting a passage, and a JSON-RPC body is a bad place for a novel.
const MAX_PREVIEW_CHARS: usize = 100_000;

/// Default words returned by `preview_timeline` — enough to see the pacing
/// and the pivots without returning the whole document twice.
const DEFAULT_PREVIEW_WORDS: usize = 40;
const MAX_PREVIEW_WORDS: usize = 500;

const DEFAULT_WPM: u32 = 350;

// ------------------------------------------------------------ JSON-RPC glue

/// A JSON-RPC error that is a *protocol* failure — bad method, bad params.
/// Tool failures are not these: they come back as a successful result with
/// `isError: true`, which is what MCP clients show to the model.
#[derive(Debug)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
    fn method_not_found(method: &str) -> Self {
        RpcError::new(-32601, format!("unknown method: {method}"))
    }
    fn invalid_params(message: impl Into<String>) -> Self {
        RpcError::new(-32602, message)
    }
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, e: RpcError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": e.code, "message": e.message },
    })
}

/// A tool result the model can read. `text` carries the JSON payload because
/// every MCP client renders text content; `structuredContent` carries the same
/// data for clients that prefer it typed.
fn tool_ok(value: Value) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        }],
        "structuredContent": value,
        "isError": false,
    })
}

/// A tool that failed for an ordinary reason — no session, a bad URL, a limit
/// reached. The model sees the message and can act on it.
fn tool_err(message: impl Into<String>) -> Value {
    let message = message.into();
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// AppError → a tool error, keeping the message the HTTP API would have given.
fn from_app_error(e: AppError) -> Value {
    let response = e.into_response();
    let status = response.status();
    tool_err(format!(
        "flick returned {}: {}",
        status.as_u16(),
        status.canonical_reason().unwrap_or("error")
    ))
}

// ---------------------------------------------------------------- tool list

fn tools() -> Value {
    json!([
        {
            "name": "preview_timeline",
            "title": "Preview the RSVP pacing of a passage",
            "description":
                "Turn text into flick's paced reading timeline: for each token, the pivot \
                 letter (optimal recognition point) and a relative duration weight, plus how \
                 long the passage takes at a given words-per-minute. Use this instead of \
                 estimating reading time yourself, and instead of computing ORP positions \
                 yourself — the weight model is the product and it lives in one place. Needs \
                 no account.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "The passage to pace. Up to 100,000 characters.",
                    },
                    "wpm": {
                        "type": "integer",
                        "minimum": 100,
                        "maximum": 1200,
                        "default": DEFAULT_WPM,
                        "description":
                            "Target words per minute. 400–500 is where most practised readers \
                             settle; above 800 is skimming.",
                    },
                    "max_words": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": MAX_PREVIEW_WORDS,
                        "default": DEFAULT_PREVIEW_WORDS,
                        "description": "How many timeline entries to return. 0 returns only the totals.",
                    },
                },
                "required": ["text"],
                "additionalProperties": false,
            },
        },
        {
            "name": "list_catalog",
            "title": "List the built-in public-domain catalog",
            "description":
                "The public-domain works shipped with this flick server — title, author, \
                 language, length and a description. Free to read, no account needed to \
                 browse. Every new user is seeded with all of them.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "search_library",
            "title": "Search the signed-in reader's library",
            "description":
                "Full-text search across the caller's own books — titles and contents. Omit \
                 the query to list the whole library. Requires a flick session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Words to search for. Omitted returns everything.",
                    },
                },
                "additionalProperties": false,
            },
        },
        {
            "name": "get_book_text",
            "title": "Read a book's text",
            "description":
                "The full text of one of the caller's books, as paragraphs. Word indices \
                 match the reading timeline exactly, so `position` from search_library points \
                 into this. Use this to actually read or summarise a book. Requires a flick \
                 session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "book_id": { "type": "string", "description": "Book id from search_library." },
                    "from_position": {
                        "type": "integer",
                        "minimum": 0,
                        "description":
                            "Skip ahead to this word index — pass the book's `position` to \
                             continue where the reader left off.",
                    },
                    "max_words": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Cap on words returned. Default 5000.",
                    },
                },
                "required": ["book_id"],
                "additionalProperties": false,
            },
        },
        {
            "name": "save_text",
            "title": "Save text to the reader's library",
            "description":
                "Add a passage to the caller's flick library so they can speed-read it later. \
                 Requires a flick session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "The text to save." },
                    "title": {
                        "type": "string",
                        "description": "Defaults to the first ~40 characters of the text.",
                    },
                },
                "required": ["text"],
                "additionalProperties": false,
            },
        },
        {
            "name": "import_url",
            "title": "Save a web article to the reader's library",
            "description":
                "Fetch a public URL and add the readable article — or the PDF/EPUB/text file \
                 behind it — to the caller's library. The fetch is SSRF-guarded: http(s) only, \
                 public addresses only, checked on every redirect. Requires a flick session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "format": "uri", "description": "Public http(s) URL." },
                    "title": { "type": "string", "description": "Override the extracted title." },
                },
                "required": ["url"],
                "additionalProperties": false,
            },
        },
        {
            "name": "reading_stats",
            "title": "The reader's stats and streak",
            "description":
                "Words read today, the all-time total, the daily goal, the current and best \
                 streak, and the last 42 days. Requires a flick session.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
        {
            "name": "server_info",
            "title": "What this flick server is",
            "description":
                "Edition (selfhost or hosted), version, public URL, and where the API \
                 documentation lives. Needs no account.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        },
    ])
}

// -------------------------------------------------------------------- tools

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|v| v as usize)
}

/// Tools that need a library refuse politely, pointing at the one request that
/// fixes it, rather than returning a bare 401 the model has to guess about.
fn needs_session(base: &str) -> Value {
    tool_err(format!(
        "This tool needs a flick session. POST {base}/api/auth/guest (no registration, no \
         credentials) and send the returned flick_session cookie with your MCP requests, or \
         sign in. See {base}/auth.md."
    ))
}

async fn preview_timeline(args: &Value) -> Result<Value, RpcError> {
    let text = arg_str(args, "text")
        .ok_or_else(|| RpcError::invalid_params("preview_timeline needs `text`"))?
        .to_string();
    if text.trim().is_empty() {
        return Ok(tool_err("`text` is empty."));
    }
    if text.chars().count() > MAX_PREVIEW_CHARS {
        return Ok(tool_err(format!(
            "`text` is longer than {MAX_PREVIEW_CHARS} characters. Save it with save_text and \
             read it from the library instead."
        )));
    }
    let wpm = args
        .get("wpm")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_WPM as u64)
        .clamp(100, 1200) as f64;
    let max_words = arg_usize(args, "max_words")
        .unwrap_or(DEFAULT_PREVIEW_WORDS)
        .min(MAX_PREVIEW_WORDS);

    // Tokenizing is CPU-bound; keep it off the async runtime, exactly as the
    // book-insert path does.
    let timeline = tokio::task::spawn_blocking(move || Timeline::from_text(&text))
        .await
        .map_err(|_| RpcError::new(-32603, "tokenizer failed"))?;

    let total_weight: f64 = timeline.words.iter().map(|w| w.2 as f64).sum();
    let ms = total_weight * (60_000.0 / wpm);
    let words: Vec<Value> = timeline
        .words
        .iter()
        .take(max_words)
        .map(|w| json!([w.0, w.1, w.2]))
        .collect();

    Ok(tool_ok(json!({
        "word_count": timeline.word_count,
        "wpm": wpm as u64,
        "estimated_ms": ms.round() as u64,
        "estimated_reading_time": human_duration(ms),
        "words": words,
        "words_truncated": timeline.words.len() > words.len(),
        "format": "Each entry is [text, orp_index, weight]. Show the token for \
                   weight * (60000 / wpm) milliseconds, with the character at orp_index \
                   held in a fixed position and highlighted.",
    })))
}

fn human_duration(ms: f64) -> String {
    let total_secs = (ms / 1000.0).round() as u64;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    if mins == 0 {
        format!("{secs}s")
    } else {
        format!("{mins}m {secs}s")
    }
}

async fn list_catalog(state: &AppState) -> Result<Value, RpcError> {
    match catalog::list(State(state.clone())).await {
        Ok(axum::Json(entries)) => Ok(tool_ok(json!({ "works": entries }))),
        Err(e) => Ok(from_app_error(e)),
    }
}

async fn search_library(state: &AppState, user: &User, args: &Value) -> Result<Value, RpcError> {
    let user_id = user.id.clone();
    let books = match arg_str(args, "query")
        .map(str::trim)
        .filter(|q| !q.is_empty())
    {
        Some(q) => {
            let Some(match_query) = books::fts_query(q) else {
                return Ok(tool_ok(json!({ "books": [] })));
            };
            state
                .db
                .call(move |c| db::search_books(c, &user_id, &match_query))
                .await
                .map_err(|_| RpcError::invalid_params("invalid search query"))?
        }
        None => match state.db.call(move |c| db::list_books(c, &user_id)).await {
            Ok(books) => books,
            Err(e) => return Ok(from_app_error(e)),
        },
    };
    Ok(tool_ok(json!({ "books": books })))
}

async fn get_book_text(state: &AppState, user: &User, args: &Value) -> Result<Value, RpcError> {
    let id = arg_str(args, "book_id")
        .ok_or_else(|| RpcError::invalid_params("get_book_text needs `book_id`"))?
        .to_string();
    let from = arg_usize(args, "from_position").unwrap_or(0);
    let limit = arg_usize(args, "max_words").unwrap_or(5000).max(1);

    let user_id = user.id.clone();
    let text = match state
        .db
        .call(move |c| db::book_text(c, &user_id, &id))
        .await
    {
        Ok(Some(text)) => text,
        Ok(None) => {
            return Ok(tool_err(
                "No such book in this library. Book ids come from search_library, and a book \
                 that belongs to someone else looks identical to one that does not exist.",
            ))
        }
        Err(e) => return Ok(from_app_error(e)),
    };

    // The same tokenizer the timeline uses, so indices line up with `position`.
    let paragraphs = tokio::task::spawn_blocking(move || flick_core::paragraphs(&text))
        .await
        .map_err(|_| RpcError::new(-32603, "tokenizer failed"))?;

    let total: usize = paragraphs.iter().map(Vec::len).sum();
    let mut skipped = 0usize;
    let mut taken = 0usize;
    let mut out: Vec<String> = Vec::new();
    for para in &paragraphs {
        if skipped + para.len() <= from {
            skipped += para.len();
            continue;
        }
        let start = from.saturating_sub(skipped);
        skipped += para.len();
        let slice = &para[start.min(para.len())..];
        let room = limit - taken;
        let slice = &slice[..slice.len().min(room)];
        taken += slice.len();
        if !slice.is_empty() {
            out.push(slice.join(" "));
        }
        if taken >= limit {
            break;
        }
    }

    Ok(tool_ok(json!({
        "paragraphs": out,
        "from_position": from,
        "words_returned": taken,
        "word_count": total,
        "more": from + taken < total,
        "next_position": from + taken,
    })))
}

async fn save_text(state: &AppState, user: &User, args: &Value) -> Result<Value, RpcError> {
    let text =
        arg_str(args, "text").ok_or_else(|| RpcError::invalid_params("save_text needs `text`"))?;
    if text.trim().is_empty() {
        return Ok(tool_err("`text` is empty."));
    }
    let prepared = crate::import::Prepared {
        title: arg_str(args, "title").map(str::to_string),
        text: text.to_string(),
        source: "paste",
        author: None,
        url: None,
        favicon: None,
        excerpt: None,
        category: None,
    };
    match books::insert_prepared(state, user, prepared).await {
        Ok(book) => Ok(tool_ok(json!({ "book": book }))),
        Err(e) => Ok(from_app_error(e)),
    }
}

async fn import_url(
    state: &AppState,
    user: &User,
    client: &str,
    args: &Value,
) -> Result<Value, RpcError> {
    let url =
        arg_str(args, "url").ok_or_else(|| RpcError::invalid_params("import_url needs `url`"))?;
    // Same bucket as POST /api/import/url: MCP is another door onto the same
    // outbound fetcher, not a second allowance.
    if let Err(retry_after) = state.limiter.check_import_url(client) {
        return Ok(tool_err(format!(
            "URL imports are rate limited; try again in {retry_after} seconds."
        )));
    }
    let title = arg_str(args, "title").map(str::to_string);
    let prepared = match books::prepare_url_import(url, title).await {
        Ok(prepared) => prepared,
        Err(e) => return Ok(from_app_error(e)),
    };
    match books::insert_prepared(state, user, prepared).await {
        Ok(book) => Ok(tool_ok(json!({ "book": book }))),
        Err(e) => Ok(from_app_error(e)),
    }
}

async fn reading_stats(state: &AppState, user: &User) -> Result<Value, RpcError> {
    match stats::stats(State(state.clone()), AuthUser(user.clone())).await {
        Ok(axum::Json(value)) => Ok(tool_ok(value)),
        Err(e) => Ok(from_app_error(e)),
    }
}

fn server_info(state: &AppState) -> Value {
    let base = &state.config.public_url;
    tool_ok(json!({
        "name": "flick",
        "version": env!("CARGO_PKG_VERSION"),
        "edition": state.config.edition.as_str(),
        "public_url": base,
        "unlimited": state.config.edition == Edition::Selfhost,
        "api_description": format!("{base}/openapi.json"),
        "api_documentation": format!("{base}/docs/api"),
        "authentication": format!("{base}/auth.md"),
        "skills": format!("{base}/.well-known/agent-skills/index.json"),
    }))
}

async fn call_tool(
    state: &AppState,
    user: Option<&User>,
    client: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("tools/call needs `name`"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let base = &state.config.public_url;

    // Public tools first; the rest need a session and say so if they lack one.
    match name {
        "preview_timeline" => return preview_timeline(&args).await,
        "list_catalog" => return list_catalog(state).await,
        "server_info" => return Ok(server_info(state)),
        _ => {}
    }

    let Some(user) = user else {
        return Ok(needs_session(base));
    };

    match name {
        "search_library" => search_library(state, user, &args).await,
        "get_book_text" => get_book_text(state, user, &args).await,
        "save_text" => save_text(state, user, &args).await,
        "import_url" => import_url(state, user, client, &args).await,
        "reading_stats" => reading_stats(state, user).await,
        other => Err(RpcError::invalid_params(format!("unknown tool: {other}"))),
    }
}

// ----------------------------------------------------------------- dispatch

fn initialize(state: &AppState) -> Value {
    let base = &state.config.public_url;
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "flick",
            "title": "flick — speed reading",
            "version": env!("CARGO_PKG_VERSION"),
            "websiteUrl": base,
        },
        "instructions": format!(
            "flick is a speed reader. `preview_timeline` paces arbitrary text — use it rather \
             than estimating reading times or pivot positions yourself. `list_catalog` and \
             `server_info` are public. The library tools (search_library, get_book_text, \
             save_text, import_url, reading_stats) act on one reader's own books and need a \
             flick_session cookie; POST {base}/api/auth/guest mints one with no registration. \
             When reporting reading speeds, note that comprehension degrades above roughly \
             500 wpm — {base}/science has the evidence."
        ),
    })
}

/// One JSON-RPC message in, at most one out. Notifications (no `id`) are
/// acknowledged with no response, per JSON-RPC.
async fn dispatch(
    state: &AppState,
    user: Option<&User>,
    client: &str,
    msg: &Value,
) -> Option<Value> {
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = msg.get("id").cloned();

    // A message with no `id` is a notification: act on nothing, answer nothing.
    let id = id.filter(|v| !v.is_null())?;

    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(initialize(state)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call_tool(state, user, client, &params).await,
        "resources/list" => Ok(json!({ "resources": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        other => Err(RpcError::method_not_found(other)),
    };

    Some(match result {
        Ok(value) => ok(id, value),
        Err(e) => err(id, e),
    })
}

fn json(status: StatusCode, value: Value) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        value.to_string(),
    )
        .into_response()
}

/// `POST /mcp` — the streamable-HTTP transport. Requests are answered with a
/// single JSON body rather than an SSE stream: every tool here returns in one
/// shot, so there is nothing to stream and no session state to keep.
pub async fn handle(
    State(state): State<AppState>,
    user: Option<AuthUser>,
    crate::ratelimit::ClientIp(client): crate::ratelimit::ClientIp,
    body: axum::body::Bytes,
) -> Response {
    let Ok(message) = serde_json::from_slice::<Value>(&body) else {
        return json(
            StatusCode::BAD_REQUEST,
            err(Value::Null, RpcError::new(-32700, "parse error")),
        );
    };
    let user = user.map(|AuthUser(u)| u);
    let user = user.as_ref();

    match message {
        Value::Array(messages) if messages.is_empty() => json(
            StatusCode::BAD_REQUEST,
            err(Value::Null, RpcError::new(-32600, "empty batch")),
        ),
        Value::Array(messages) => {
            let mut out = Vec::new();
            for msg in &messages {
                if let Some(response) = dispatch(&state, user, &client, msg).await {
                    out.push(response);
                }
            }
            if out.is_empty() {
                // Nothing but notifications.
                StatusCode::ACCEPTED.into_response()
            } else {
                json(StatusCode::OK, Value::Array(out))
            }
        }
        msg => match dispatch(&state, user, &client, &msg).await {
            Some(response) => json(StatusCode::OK, response),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

/// `GET /mcp` — this transport has no server-initiated stream to open.
async fn no_stream() -> Response {
    json(
        StatusCode::METHOD_NOT_ALLOWED,
        err(
            Value::Null,
            RpcError::new(-32600, "this MCP endpoint accepts POST only"),
        ),
    )
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/mcp", post(handle).get(no_stream))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: Value) -> Value {
        v
    }

    #[tokio::test]
    async fn preview_paces_text_and_truncates_words() {
        let result = preview_timeline(&args(json!({
            "text": "The quick brown fox jumps over the lazy dog.",
            "wpm": 300,
            "max_words": 3,
        })))
        .await
        .expect("tool runs");
        let out = &result["structuredContent"];
        assert_eq!(out["word_count"], 9);
        assert_eq!(out["wpm"], 300);
        assert_eq!(out["words"].as_array().unwrap().len(), 3);
        assert_eq!(out["words_truncated"], true);
        assert!(out["estimated_ms"].as_u64().unwrap() > 0);
        // [text, orp_index, weight]
        assert_eq!(out["words"][0][0], "The");
    }

    #[tokio::test]
    async fn preview_rejects_empty_and_oversized_text() {
        let empty = preview_timeline(&args(json!({ "text": "   " })))
            .await
            .unwrap();
        assert_eq!(empty["isError"], true);

        let huge = "word ".repeat(MAX_PREVIEW_CHARS);
        let over = preview_timeline(&args(json!({ "text": huge })))
            .await
            .unwrap();
        assert_eq!(over["isError"], true);
    }

    #[tokio::test]
    async fn missing_text_is_a_protocol_error_not_a_tool_error() {
        let e = preview_timeline(&args(json!({ "wpm": 400 })))
            .await
            .unwrap_err();
        assert_eq!(e.code, -32602);
    }

    #[test]
    fn every_tool_declares_an_object_schema() {
        for tool in tools().as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            assert!(!tool["description"].as_str().unwrap().is_empty(), "{name}");
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        }
    }

    #[test]
    fn wpm_is_clamped_to_the_contract_range() {
        for (input, expected) in [(50u64, 100u64), (5000, 1200), (450, 450)] {
            let clamped = input.clamp(100, 1200);
            assert_eq!(clamped, expected);
        }
    }

    #[test]
    fn durations_read_like_durations() {
        assert_eq!(human_duration(4_000.0), "4s");
        assert_eq!(human_duration(125_000.0), "2m 5s");
    }
}
