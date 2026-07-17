//! Integration tests: the router is exercised in-process with
//! `tower::ServiceExt::oneshot` against a temp-dir SQLite database.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use flick_server::config::{Config, OAuthCreds};
use flick_server::db::Db;
use flick_server::{app, AppState};

fn test_app_with_state() -> (Router, AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        addr: "127.0.0.1:0".into(),
        data_dir: dir.path().join("data"),
        public_url: "http://localhost:8484".into(),
        web_dist: dir.path().join("no-such-dist"),
        oidc: None,
        oidc_name: "SSO".into(),
        oauth_google: None,
        oauth_github: None,
        smtp_url: None,
        smtp_from: "flick <no-reply@localhost>".into(),
        dropbox_app_key: None,
        google_picker_api_key: None,
    };
    let db = Db::open(&config.data_dir).expect("open db");
    let state = AppState::new(db, config);
    (app(state.clone()), state, dir)
}

fn test_app() -> (Router, tempfile::TempDir) {
    let (app, _state, dir) = test_app_with_state();
    (app, dir)
}

/// A test app whose `Config` is customized (used for the integrations
/// endpoint, which reflects configured keys).
fn test_app_with_config(mutate: impl FnOnce(&mut Config)) -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = Config {
        addr: "127.0.0.1:0".into(),
        data_dir: dir.path().join("data"),
        public_url: "http://localhost:8484".into(),
        web_dist: dir.path().join("no-such-dist"),
        oidc: None,
        oidc_name: "SSO".into(),
        oauth_google: None,
        oauth_github: None,
        smtp_url: None,
        smtp_from: "flick <no-reply@localhost>".into(),
        dropbox_app_key: None,
        google_picker_api_key: None,
    };
    mutate(&mut config);
    let db = Db::open(&config.data_dir).expect("open db");
    let state = AppState::new(db, config);
    (app(state), dir)
}

async fn send(app: &Router, req: Request<Body>) -> Response {
    app.clone().oneshot(req).await.expect("infallible")
}

fn json_request(method: &str, uri: &str, cookie: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    builder.body(Body::from(body.to_string())).expect("request")
}

fn bare_request(method: &str, uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header(header::COOKIE, c);
    }
    builder.body(Body::empty()).expect("request")
}

/// A `POST /api/books` multipart upload of `bytes` as `file` (filename only,
/// no explicit Content-Type field — the server sniffs the bytes).
fn upload_request(cookie: &str, filename: &str, bytes: &[u8]) -> Request<Body> {
    let boundary = "XFLICKBOUNDARY";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri("/api/books")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::COOKIE, cookie)
        .body(Body::from(body))
        .expect("request")
}

/// Extract `flick_session=...` from Set-Cookie for reuse as a Cookie header.
fn session_cookie(resp: &Response) -> String {
    let set = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("flick_session="))
        .expect("session set-cookie");
    set.split(';').next().expect("cookie pair").to_string()
}

async fn body_json(resp: Response) -> Value {
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    serde_json::from_slice(&bytes).expect("json body")
}

async fn register(app: &Router, email: &str) -> String {
    let resp = send(
        app,
        json_request(
            "POST",
            "/api/auth/register",
            None,
            json!({"email": email, "password": "hunter22hunter22", "name": "Tester"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    session_cookie(&resp)
}

async fn create_paste_book(app: &Router, cookie: &str, title: Option<&str>, text: &str) -> Value {
    let mut body = json!({ "text": text });
    if let Some(t) = title {
        body["title"] = json!(t);
    }
    let resp = send(app, json_request("POST", "/api/books", Some(cookie), body)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await
}

// ------------------------------------------------------------------ auth

#[tokio::test]
async fn register_login_logout_me_flow() {
    let (app, _dir) = test_app();

    // register sets a session and returns the user object
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/register",
            None,
            json!({"email": "A@Example.com", "password": "hunter22hunter22", "name": "Ada"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let cookie = session_cookie(&resp);
    let user = body_json(resp).await;
    assert_eq!(user["email"], "a@example.com"); // normalized
    assert_eq!(user["name"], "Ada");
    assert!(user["id"].as_str().is_some_and(|id| !id.is_empty()));

    // me with the cookie
    let resp = send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["email"], "a@example.com");

    // logout clears the cookie and invalidates the session
    let resp = send(&app, bare_request("POST", "/api/auth/logout", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let cleared = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .expect("clear cookie");
    assert!(cleared.contains("Max-Age=0"));
    let resp = send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // login again
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/login",
            None,
            json!({"email": "a@example.com", "password": "hunter22hunter22"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = session_cookie(&resp);
    let resp = send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // wrong password and unknown email are indistinguishable 401s
    for (email, pw) in [
        ("a@example.com", "wrong-password"),
        ("nobody@example.com", "hunter22hunter22"),
    ] {
        let resp = send(
            &app,
            json_request(
                "POST",
                "/api/auth/login",
                None,
                json!({"email": email, "password": pw}),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_json(resp).await["error"], "invalid email or password");
    }
}

#[tokio::test]
async fn duplicate_register_conflicts() {
    let (app, _dir) = test_app();
    register(&app, "dup@example.com").await;
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/register",
            None,
            json!({"email": "DUP@example.com", "password": "hunter22hunter22", "name": "Two"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(body_json(resp).await["error"].is_string());
}

#[tokio::test]
async fn auth_required_401s_are_json() {
    let (app, _dir) = test_app();
    for req in [
        bare_request("GET", "/api/auth/me", None),
        bare_request("GET", "/api/books", None),
        json_request("POST", "/api/books", None, json!({"text": "hi"})),
        bare_request("GET", "/api/books/xyz/timeline", None),
        json_request("PUT", "/api/books/xyz/position", None, json!({"position": 0})),
        bare_request("DELETE", "/api/books/xyz", None),
    ] {
        let resp = send(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(body_json(resp).await["error"], "authentication required");
    }
}

#[tokio::test]
async fn bad_json_bodies_get_json_errors() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "json@example.com").await;

    // syntactically broken JSON
    let req = Request::builder()
        .method("POST")
        .uri("/api/books")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, &cookie)
        .body(Body::from("{not json"))
        .expect("request");
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(resp).await["error"].is_string());

    // structurally wrong JSON (missing "text")
    let resp = send(
        &app,
        json_request("POST", "/api/books", Some(&cookie), json!({"title": "x"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // empty text
    let resp = send(
        &app,
        json_request("POST", "/api/books", Some(&cookie), json!({"text": "  "})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"], "text must not be empty");
}

// ----------------------------------------------------------------- books

#[tokio::test]
async fn books_full_flow() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "reader@example.com").await;

    // create with explicit title
    let book = create_paste_book(
        &app,
        &cookie,
        Some("Speed"),
        "Reading fast, is fun.\n\nSecond paragraph here.",
    )
    .await;
    assert_eq!(book["title"], "Speed");
    assert_eq!(book["source"], "paste");
    assert_eq!(book["word_count"], 7);
    assert_eq!(book["position"], 0);
    assert!(book["created_at"].as_i64().is_some());
    let id = book["id"].as_str().expect("id").to_string();

    // timeline matches the contract shape and flick-core output
    let resp = send(
        &app,
        bare_request("GET", &format!("/api/books/{id}/timeline"), Some(&cookie)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let timeline = body_json(resp).await;
    assert_eq!(timeline["version"], 1);
    assert_eq!(timeline["word_count"], 7);
    let words = timeline["words"].as_array().expect("words");
    assert_eq!(words.len(), 7);
    assert_eq!(words[0], json!(["Reading", 2, 1.0]));
    assert_eq!(words[1], json!(["fast,", 1, 1.6]));
    assert_eq!(words[3], json!(["fun.", 1, 2.6])); // paragraph dwell

    // update position, visible in GET
    let resp = send(
        &app,
        json_request(
            "PUT",
            &format!("/api/books/{id}/position"),
            Some(&cookie),
            json!({"position": 3}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(&app, bare_request("GET", &format!("/api/books/{id}"), Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["position"], 3);

    // out-of-range / negative positions rejected
    for pos in [json!(999), json!(-1)] {
        let resp = send(
            &app,
            json_request(
                "PUT",
                &format!("/api/books/{id}/position"),
                Some(&cookie),
                json!({"position": pos}),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // list contains it (plus the seeded intro book)
    let resp = send(&app, bare_request("GET", "/api/books", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    assert_eq!(list.as_array().map(Vec::len), Some(2));
    assert!(list
        .as_array()
        .expect("array")
        .iter()
        .any(|b| b["id"] == id.as_str()));

    // delete, then everything 404s
    let resp = send(&app, bare_request("DELETE", &format!("/api/books/{id}"), Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(&app, bare_request("GET", "/api/books", Some(&cookie))).await;
    let remaining = body_json(resp).await;
    let remaining = remaining.as_array().expect("array");
    assert!(remaining.iter().all(|b| b["id"] != id.as_str()));
    for req in [
        bare_request("GET", &format!("/api/books/{id}"), Some(&cookie)),
        bare_request("GET", &format!("/api/books/{id}/timeline"), Some(&cookie)),
        bare_request("DELETE", &format!("/api/books/{id}"), Some(&cookie)),
    ] {
        let resp = send(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_json(resp).await["error"], "not found");
    }
}

#[tokio::test]
async fn paste_without_title_defaults_to_first_40_chars() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "title@example.com").await;
    let text = "The quick brown fox jumps over the lazy dog again and again and again.";
    let book = create_paste_book(&app, &cookie, None, text).await;
    let title = book["title"].as_str().expect("title");
    assert!(title.chars().count() <= 40, "title too long: {title:?}");
    assert!(text.starts_with(title) || title.chars().count() >= 30);
}

#[tokio::test]
async fn foreign_books_are_404() {
    let (app, _dir) = test_app();
    let alice = register(&app, "alice@example.com").await;
    let bob = register(&app, "bob@example.com").await;

    let book = create_paste_book(&app, &alice, Some("Private"), "Alice's secret text.").await;
    let id = book["id"].as_str().expect("id").to_string();

    for req in [
        bare_request("GET", &format!("/api/books/{id}"), Some(&bob)),
        bare_request("GET", &format!("/api/books/{id}/timeline"), Some(&bob)),
        json_request(
            "PUT",
            &format!("/api/books/{id}/position"),
            Some(&bob),
            json!({"position": 1}),
        ),
        bare_request("DELETE", &format!("/api/books/{id}"), Some(&bob)),
    ] {
        let resp = send(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
    // Bob sees only his own seeded intro book; Alice's book stays hers.
    let resp = send(&app, bare_request("GET", "/api/books", Some(&bob))).await;
    let bobs = body_json(resp).await;
    let bobs = bobs.as_array().expect("array");
    assert!(bobs.iter().all(|b| b["id"] != id.as_str()));
    let resp = send(&app, bare_request("GET", "/api/books", Some(&alice))).await;
    let alices = body_json(resp).await;
    assert!(alices
        .as_array()
        .expect("array")
        .iter()
        .any(|b| b["id"] == id.as_str()));
}

#[tokio::test]
async fn txt_upload_accepted_and_binary_rejected() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "upload@example.com").await;

    // A plain-text upload is now a first-class import (source "txt"); the
    // filename supplies the title when no title field is present.
    let resp = send(
        &app,
        upload_request(&cookie, "notes.txt", b"Just some plain text to read here."),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let book = body_json(resp).await;
    assert_eq!(book["source"], "txt");
    assert_eq!(book["title"], "notes");
    assert_eq!(book["category"], "docs");

    // Non-UTF-8 binary that is neither PDF nor EPUB is rejected.
    let resp = send(
        &app,
        upload_request(&cookie, "blob.bin", &[0x00, 0xFF, 0xFE, 0x01, 0x02, 0x80]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(resp).await["error"].is_string());
}

// ------------------------------------------------------------ misc routes

#[tokio::test]
async fn providers_reflects_disabled_oidc() {
    let (app, _dir) = test_app();
    let resp = send(&app, bare_request("GET", "/api/auth/providers", None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body, json!({"providers": []}));

    // OAuth logins are 404 when unconfigured (alias + registry routes), and
    // unknown providers are 404 always.
    for uri in [
        "/api/auth/oidc/login",
        "/api/auth/oauth/oidc/login",
        "/api/auth/oauth/google/login",
        "/api/auth/oauth/github/login",
        "/api/auth/oauth/myspace/login",
    ] {
        let resp = send(&app, bare_request("GET", uri, None)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn unknown_api_route_is_json_404() {
    let (app, _dir) = test_app();
    let resp = send(&app, bare_request("GET", "/api/definitely/not/here", None)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["error"], "not found");
}

#[tokio::test]
async fn missing_web_dist_serves_plain_text() {
    let (app, _dir) = test_app();
    let resp = send(&app, bare_request("GET", "/", None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    assert!(bytes.starts_with(b"flick-server: web dist not found"));
}

// ------------------------------------------------------- v0.2: profile

#[tokio::test]
async fn new_user_defaults_and_starter_book() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "fresh@example.com").await;

    let me = body_json(send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await).await;
    assert_eq!(me["onboarded"], false);
    assert_eq!(me["username"], Value::Null);
    assert_eq!(me["guest"], false);
    assert_eq!(me["settings"]["wpm"], 350);
    assert_eq!(me["settings"]["theme"], "auto");
    assert_eq!(me["settings"]["accent"], "red");
    assert_eq!(me["settings"]["lang"], "auto");

    let books =
        body_json(send(&app, bare_request("GET", "/api/books", Some(&cookie))).await).await;
    let books = books.as_array().expect("array");
    assert_eq!(books.len(), 1);
    assert_eq!(books[0]["source"], "intro");
    assert_eq!(books[0]["title"], "Welcome to flick");
    assert!(books[0]["word_count"].as_i64().expect("count") > 100);

    // The intro book has a playable timeline like any other book.
    let id = books[0]["id"].as_str().expect("id");
    let resp = send(
        &app,
        bare_request("GET", &format!("/api/books/{id}/timeline"), Some(&cookie)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tl = body_json(resp).await;
    assert_eq!(tl["version"], 1);
}

#[tokio::test]
async fn patch_me_full_onboarding() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "onboard@example.com").await;

    let resp = send(
        &app,
        json_request(
            "PATCH",
            "/api/auth/me",
            Some(&cookie),
            json!({
                "username": "phil_22",
                "onboarded": true,
                "settings": {"wpm": 425, "theme": "dark"}
            }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me = body_json(resp).await;
    assert_eq!(me["username"], "phil_22");
    assert_eq!(me["onboarded"], true);
    assert_eq!(me["settings"]["wpm"], 425);
    assert_eq!(me["settings"]["theme"], "dark");

    // Persisted, not just echoed.
    let me = body_json(send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await).await;
    assert_eq!(me["username"], "phil_22");
    assert_eq!(me["onboarded"], true);
    assert_eq!(me["settings"]["wpm"], 425);
}

#[tokio::test]
async fn patch_me_validation() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "invalid@example.com").await;

    for (body, needle) in [
        (json!({"username": "x"}), "username"),
        (json!({"username": "has spaces"}), "username"),
        (json!({"settings": {"wpm": 50}}), "wpm"),
        (json!({"settings": {"wpm": 5000}}), "wpm"),
        (json!({"settings": {"theme": "neon"}}), "theme"),
        (json!({"name": "   "}), "name"),
    ] {
        let resp = send(&app, json_request("PATCH", "/api/auth/me", Some(&cookie), body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let err = body_json(resp).await;
        assert!(
            err["error"].as_str().expect("msg").contains(needle),
            "error should mention {needle}: {err}"
        );
    }

    // Nothing partial was applied.
    let me = body_json(send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await).await;
    assert_eq!(me["username"], Value::Null);
    assert_eq!(me["settings"]["wpm"], 350);
}

// --------------------------------------------------------- v0.3: guests

#[tokio::test]
async fn guest_create_first_add_and_merge_on_register() {
    let (app, _dir) = test_app();

    // Anonymous session: a real user row, no email, no intro book yet.
    let resp = send(&app, bare_request("POST", "/api/auth/guest", None)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let guest_cookie = session_cookie(&resp);
    let guest = body_json(resp).await;
    assert_eq!(guest["guest"], true);
    assert_eq!(guest["email"], Value::Null);
    assert_eq!(guest["name"], "READER");
    let books =
        body_json(send(&app, bare_request("GET", "/api/books", Some(&guest_cookie))).await).await;
    assert_eq!(books.as_array().map(Vec::len), Some(0));

    // First add seeds the intro book alongside it.
    let book = create_paste_book(&app, &guest_cookie, Some("Mine"), "Guest words go here.").await;
    let id = book["id"].as_str().expect("id").to_string();
    let books =
        body_json(send(&app, bare_request("GET", "/api/books", Some(&guest_cookie))).await).await;
    let books = books.as_array().expect("array");
    assert_eq!(books.len(), 2);
    assert_eq!(books.iter().filter(|b| b["source"] == "intro").count(), 1);

    // A second add does NOT seed another intro.
    create_paste_book(&app, &guest_cookie, Some("More"), "Even more guest words.").await;
    let books =
        body_json(send(&app, bare_request("GET", "/api/books", Some(&guest_cookie))).await).await;
    assert_eq!(books.as_array().map(Vec::len), Some(3));

    // Reading as a guest: position + stats live on the guest row.
    let resp = send(
        &app,
        json_request(
            "PUT",
            &format!("/api/books/{id}/position"),
            Some(&guest_cookie),
            json!({"position": 2, "read": 350}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Register while the guest cookie is present → everything merges.
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/register",
            Some(&guest_cookie),
            json!({"email": "merged@example.com", "password": "hunter22hunter22", "name": "M"}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let cookie = session_cookie(&resp);
    let user = body_json(resp).await;
    assert_eq!(user["guest"], false);
    assert_eq!(user["email"], "merged@example.com");

    // Books moved over; exactly ONE intro book (no duplicate).
    let books = body_json(send(&app, bare_request("GET", "/api/books", Some(&cookie))).await).await;
    let books = books.as_array().expect("array");
    assert_eq!(books.len(), 3); // intro + Mine + More
    assert_eq!(books.iter().filter(|b| b["source"] == "intro").count(), 1);
    assert!(books.iter().any(|b| b["id"] == id.as_str()));

    // Stats moved over too.
    let stats = body_json(send(&app, bare_request("GET", "/api/stats", Some(&cookie))).await).await;
    assert_eq!(stats["total_words"], 350);

    // The guest row is gone: its session no longer resolves.
    let resp = send(&app, bare_request("GET", "/api/auth/me", Some(&guest_cookie))).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ----------------------------------------------- v0.3: email-first flow

#[tokio::test]
async fn lookup_known_and_unknown_email() {
    let (app, _dir) = test_app();
    register(&app, "known@example.com").await;

    let resp = send(
        &app,
        json_request("POST", "/api/auth/lookup", None, json!({"email": "KNOWN@example.com"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["exists"], true);
    assert_eq!(body["methods"], json!(["password", "code"]));

    let resp = send(
        &app,
        json_request("POST", "/api/auth/lookup", None, json!({"email": "nobody@example.com"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["exists"], false);
    assert_eq!(body["methods"], json!([]));
}

#[tokio::test]
async fn login_code_roundtrip() {
    let (app, state, _dir) = test_app_with_state();
    register(&app, "code@example.com").await;

    // The request endpoint is a silent 204 for known and unknown emails.
    for email in ["code@example.com", "nobody@example.com"] {
        let resp = send(
            &app,
            json_request("POST", "/api/auth/code/request", None, json!({"email": email})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    // Mint a known code directly (the handler only ever logs/mails it).
    let code = flick_server::auth::issue_login_code(&state.db, "code@example.com")
        .await
        .expect("issue code");
    assert_eq!(code.len(), 6);

    // Wrong code, unknown email → identical 400s.
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for (email, c) in [("code@example.com", wrong), ("nobody@example.com", &code)] {
        let resp = send(
            &app,
            json_request("POST", "/api/auth/code/verify", None, json!({"email": email, "code": c})),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "invalid code");
    }

    // Correct code logs in and sets a session.
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/code/verify",
            None,
            json!({"email": "code@example.com", "code": code}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = session_cookie(&resp);
    assert_eq!(body_json(resp).await["email"], "code@example.com");
    let resp = send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Single-use: the same code is dead now.
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/code/verify",
            None,
            json!({"email": "code@example.com", "code": code}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Five bad attempts invalidate a code even if the sixth try is correct.
    let code = flick_server::auth::issue_login_code(&state.db, "code@example.com")
        .await
        .expect("issue code");
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for _ in 0..5 {
        let resp = send(
            &app,
            json_request(
                "POST",
                "/api/auth/code/verify",
                None,
                json!({"email": "code@example.com", "code": wrong}),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/auth/code/verify",
            None,
            json!({"email": "code@example.com", "code": code}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ------------------------------------------------ v0.3: accent + lang

#[tokio::test]
async fn patch_me_accent_and_lang() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "accent@example.com").await;

    let resp = send(
        &app,
        json_request(
            "PATCH",
            "/api/auth/me",
            Some(&cookie),
            json!({"settings": {"accent": "cyan", "lang": "de"}}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me = body_json(resp).await;
    assert_eq!(me["settings"]["accent"], "cyan");
    assert_eq!(me["settings"]["lang"], "de");

    // Persisted, not just echoed.
    let me = body_json(send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await).await;
    assert_eq!(me["settings"]["accent"], "cyan");
    assert_eq!(me["settings"]["lang"], "de");

    for (body, needle) in [
        (json!({"settings": {"accent": "pink"}}), "accent"),
        (json!({"settings": {"lang": "fr"}}), "lang"),
    ] {
        let resp = send(&app, json_request("PATCH", "/api/auth/me", Some(&cookie), body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let err = body_json(resp).await;
        assert!(
            err["error"].as_str().expect("msg").contains(needle),
            "error should mention {needle}: {err}"
        );
    }
}

// ---------------------------------------------------------- v0.3: stats

#[tokio::test]
async fn stats_accumulate_and_streak() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "stats@example.com").await;
    let book = create_paste_book(&app, &cookie, Some("S"), "Some words to read here.").await;
    let id = book["id"].as_str().expect("id").to_string();
    let uri = format!("/api/books/{id}/position");

    // Report reads for yesterday and today (both above the 300 goal).
    for (day, read) in [
        (Some(flick_server::stats::utc_day(-1)), 400),
        (None, 350),
    ] {
        let mut body = json!({"position": 1, "read": read});
        if let Some(d) = day {
            body["day"] = json!(d);
        }
        let resp = send(&app, json_request("PUT", &uri, Some(&cookie), body)).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    let stats = body_json(send(&app, bare_request("GET", "/api/stats", Some(&cookie))).await).await;
    assert_eq!(stats["goal"], 300);
    assert_eq!(stats["today"]["day"], flick_server::stats::utc_day(0));
    assert_eq!(stats["today"]["words"], 350);
    assert_eq!(stats["total_words"], 750);
    assert_eq!(stats["streak"]["current"], 2);
    assert_eq!(stats["streak"]["best"], 2);
    let days = stats["days"].as_array().expect("days");
    assert_eq!(days.len(), 2);
    assert_eq!(days[0]["words"], 400); // oldest first
    assert_eq!(days[1]["words"], 350);

    // read is clamped to 500 per report.
    let resp = send(
        &app,
        json_request("PUT", &uri, Some(&cookie), json!({"position": 1, "read": 9999})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let stats = body_json(send(&app, bare_request("GET", "/api/stats", Some(&cookie))).await).await;
    assert_eq!(stats["today"]["words"], 850);

    // Days too far from the server date (and garbage days) are rejected.
    for day in [
        json!(flick_server::stats::utc_day(-5)),
        json!(flick_server::stats::utc_day(3)),
        json!("2026-02-30"),
        json!("not-a-date"),
    ] {
        let resp = send(
            &app,
            json_request(
                "PUT",
                &uri,
                Some(&cookie),
                json!({"position": 1, "read": 10, "day": day}),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "day {day}");
    }
}

#[tokio::test]
async fn position_bumps_last_read_at() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "lastread@example.com").await;
    let book = create_paste_book(&app, &cookie, Some("L"), "A few words to read.").await;
    let id = book["id"].as_str().expect("id").to_string();
    assert_eq!(book["last_read_at"], Value::Null);

    let resp = send(
        &app,
        json_request(
            "PUT",
            &format!("/api/books/{id}/position"),
            Some(&cookie),
            json!({"position": 2}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let got = body_json(send(&app, bare_request("GET", &format!("/api/books/{id}"), Some(&cookie))).await)
        .await;
    let last_read = got["last_read_at"].as_i64().expect("last_read_at set");
    assert!(last_read >= got["created_at"].as_i64().expect("created_at"));
}

// ------------------------------------------------------- v0.3: sessions

#[tokio::test]
async fn sessions_post_list_and_clamps() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "sessions@example.com").await;
    let book = create_paste_book(&app, &cookie, Some("Session Book"), "Words for a session.").await;
    let id = book["id"].as_str().expect("id").to_string();

    // A sane session is stored as-is.
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/sessions",
            Some(&cookie),
            json!({"book_id": id, "started_at": 1700000000, "duration_ms": 60000, "words": 300, "avg_wpm": 300}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    // An absurd one gets sanity-clamped (duration ≤ 6h, wpm ≤ 1500,
    // words within duration×wpm ±50%).
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/sessions",
            Some(&cookie),
            json!({"book_id": id, "started_at": 1700000001, "duration_ms": 99999999999i64, "words": 1, "avg_wpm": 99999}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    let list = body_json(send(&app, bare_request("GET", "/api/sessions?limit=50", Some(&cookie))).await)
        .await;
    let list = list.as_array().expect("array");
    assert_eq!(list.len(), 2);
    // Newest first.
    assert_eq!(list[0]["started_at"], 1700000001);
    assert_eq!(list[0]["duration_ms"], 6 * 60 * 60 * 1000);
    assert_eq!(list[0]["avg_wpm"], 1500);
    // 6h at 1500 wpm = 540000 expected words; 1 clamps to the −50% floor.
    assert_eq!(list[0]["words"], 270000);
    assert_eq!(list[1]["book_title"], "Session Book");
    assert_eq!(list[1]["words"], 300);

    // Negative inputs are rejected outright.
    let resp = send(
        &app,
        json_request(
            "POST",
            "/api/sessions",
            Some(&cookie),
            json!({"book_id": id, "started_at": 0, "duration_ms": -5, "words": 10, "avg_wpm": 300}),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Deleting the book keeps the feed working with a DELETED marker.
    let resp = send(&app, bare_request("DELETE", &format!("/api/books/{id}"), Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let list = body_json(send(&app, bare_request("GET", "/api/sessions", Some(&cookie))).await).await;
    assert_eq!(list[0]["book_title"], "DELETED");
    assert_eq!(list[1]["book_title"], "DELETED");
}

// -------------------------------------------------------- v0.3: catalog

#[tokio::test]
async fn catalog_list_add_and_duplicate() {
    let (app, _dir) = test_app();

    // Public: no auth required, every entry carries a word count.
    let resp = send(&app, bare_request("GET", "/api/catalog", None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let catalog = body_json(resp).await;
    let catalog = catalog.as_array().expect("array");
    assert_eq!(catalog.len(), 9);
    for entry in catalog {
        assert!(entry["word_count"].as_i64().expect("word_count") > 0, "{entry}");
    }
    let magi = catalog
        .iter()
        .find(|e| e["slug"] == "gift-of-the-magi")
        .expect("magi in catalog");
    assert_eq!(magi["author"], "O. Henry");
    assert_eq!(magi["kind"], "story");

    // Adding requires auth.
    let resp = send(&app, bare_request("POST", "/api/catalog/gift-of-the-magi/add", None)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let cookie = register(&app, "catalog@example.com").await;
    let resp = send(
        &app,
        bare_request("POST", "/api/catalog/gift-of-the-magi/add", Some(&cookie)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let book = body_json(resp).await;
    assert_eq!(book["source"], "catalog");
    assert_eq!(book["title"], "The Gift of the Magi");
    assert_eq!(book["author"], "O. Henry");
    assert_eq!(book["category"], "story");
    let book_id = book["id"].as_str().expect("id").to_string();
    assert_eq!(book["word_count"], magi["word_count"]);

    // The copied timeline is playable.
    let resp = send(
        &app,
        bare_request("GET", &format!("/api/books/{book_id}/timeline"), Some(&cookie)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["version"], 1);

    // Duplicate add → 409 carrying the existing book id.
    let resp = send(
        &app,
        bare_request("POST", "/api/catalog/gift-of-the-magi/add", Some(&cookie)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(body["error"].is_string());
    assert_eq!(body["book_id"], book_id.as_str());

    // Unknown slug → 404. Novella kind maps to the "book" category.
    let resp = send(&app, bare_request("POST", "/api/catalog/nope/add", Some(&cookie))).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = send(
        &app,
        bare_request("POST", "/api/catalog/die-verwandlung/add", Some(&cookie)),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["category"], "book");
}

// ------------------------------------------------- v0.3b: imports & search

#[tokio::test]
async fn epub_upload_extracts_text_and_metadata() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "epub@example.com").await;

    let bytes = include_bytes!("fixtures/minimal.epub");
    let resp = send(&app, upload_request(&cookie, "book.epub", bytes)).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "epub upload should succeed");
    let book = body_json(resp).await;
    assert_eq!(book["source"], "epub");
    assert_eq!(book["category"], "book");
    assert_eq!(book["title"], "The Test EPUB"); // from EPUB metadata
    assert_eq!(book["author"], "Jane Author");
    assert!(book["word_count"].as_i64().expect("count") > 0);

    // The spine text made it in: search finds a chapter body word.
    let id = book["id"].as_str().expect("id").to_string();
    let text = body_json(send(&app, bare_request("GET", &format!("/api/books/{id}/text"), Some(&cookie))).await).await;
    let flat: Vec<String> = text["paragraphs"]
        .as_array()
        .expect("paragraphs")
        .iter()
        .flat_map(|p| p.as_array().expect("para").iter().map(|w| w.as_str().expect("word").to_string()))
        .collect();
    assert!(flat.iter().any(|w| w.contains("harbor")), "expected 'harbor' in {flat:?}");
    assert!(flat.iter().any(|w| w.contains("letter")), "expected ch2 text too");
}

#[tokio::test]
async fn clippings_upload_multi_book() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "clip@example.com").await;

    let clippings = "\
The Pragmatic Programmer (Hunt, Andrew)
- Your Highlight on page 12 | Location 145-146 | Added on Monday

Care about your craft.
==========
Meditations (Marcus Aurelius)
- Your Highlight on Location 55-56 | Added on Tuesday

Waste no more time arguing about what a good man should be. Be one.
==========
The Pragmatic Programmer (Hunt, Andrew)
- Your Highlight on page 40 | Location 500-501 | Added on Wednesday

Don't live with broken windows.
==========
";
    let resp = send(&app, upload_request(&cookie, "My Clippings.txt", clippings.as_bytes())).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let book = body_json(resp).await;
    assert_eq!(book["source"], "clippings");
    assert_eq!(book["category"], "docs");
    // Spans multiple books → generic title, each highlight prefixed by book.
    assert_eq!(book["title"], "Kindle Clippings");
    let id = book["id"].as_str().expect("id").to_string();

    let text = body_json(send(&app, bare_request("GET", &format!("/api/books/{id}/text"), Some(&cookie))).await).await;
    let paras = text["paragraphs"].as_array().expect("paragraphs");
    assert_eq!(paras.len(), 3, "one paragraph per highlight");
    // First paragraph starts with its source-book prefix.
    let first_word = paras[0][0].as_str().expect("word");
    assert!(first_word.starts_with("The") , "expected book-title prefix, got {first_word:?}");
}

#[tokio::test]
async fn import_html_extracts_article() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "html@example.com").await;

    let html = "\
<!DOCTYPE html><html><head><title>Reading Faster — The Blog</title>
<link rel=\"icon\" href=\"/icon.png\"></head>
<body><nav>Home About Contact</nav>
<article>
<h1>How Speed Reading Works</h1>
<p class=\"byline\">By Jane Reader</p>
<p>Speed reading is the practice of recognizing and absorbing phrases or sentences on a page all at once, rather than identifying individual words. Skilled readers train their eyes to move efficiently across the text.</p>
<p>The optimal recognition point is the spot within a word where the eye most naturally lands. By fixing attention there, a reader can process each word with far less effort and much greater speed than before.</p>
<p>With consistent practice over several weeks, most people can comfortably double their original reading pace while keeping strong comprehension of the material they consume every day.</p>
</article>
<footer>Copyright 2026</footer></body></html>";

    let resp = send(
        &app,
        json_request("POST", "/api/import/html", Some(&cookie), json!({
            "url": "https://blog.example.com/speed-reading",
            "html": html,
        })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED, "readability import should succeed");
    let book = body_json(resp).await;
    assert_eq!(book["source"], "html");
    assert_eq!(book["category"], "article");
    assert_eq!(book["url"], "https://blog.example.com/speed-reading");
    // Favicon falls back to the origin when we cannot glean one.
    assert!(book["favicon"].as_str().expect("favicon").starts_with("https://blog.example.com"));
    assert!(!book["excerpt"].as_str().expect("excerpt").is_empty());
    assert!(book["word_count"].as_i64().expect("count") > 20);

    // The article body is searchable and the nav/footer chrome was dropped.
    let resp = send(&app, bare_request("GET", "/api/books?q=recognition", Some(&cookie))).await;
    let hits = body_json(resp).await;
    assert!(hits.as_array().expect("array").iter().any(|b| b["source"] == "html"));
}

#[tokio::test]
async fn import_url_rejects_private_and_local_addresses() {
    // The SSRF guard must reject non-public targets WITHOUT fetching. We test
    // the guard directly (no network) plus the endpoint's 400.
    for url in [
        "http://127.0.0.1/secret",
        "http://localhost:8484/api/stats",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5/",
        "http://192.168.1.1/",
        "http://[::1]/",
        "ftp://example.com/file",
    ] {
        let err = flick_server::import::guarded_fetch(url).await;
        assert!(err.is_err(), "guard should reject {url}");
    }

    // Public unicast passes the IP check; private/reserved fail it.
    use std::net::IpAddr;
    for ip in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
        assert!(flick_server::import::ip_is_global(&ip.parse::<IpAddr>().unwrap()), "{ip} should be global");
    }
    for ip in ["127.0.0.1", "10.0.0.1", "192.168.0.1", "169.254.0.1", "172.16.0.1", "100.64.0.1", "::1", "fe80::1", "fc00::1", "0.0.0.0"] {
        assert!(!flick_server::import::ip_is_global(&ip.parse::<IpAddr>().unwrap()), "{ip} should NOT be global");
    }

    // The endpoint surfaces the guard as a 400 (no outbound request made).
    let (app, _dir) = test_app();
    let cookie = register(&app, "ssrf@example.com").await;
    let resp = send(
        &app,
        json_request("POST", "/api/import/url", Some(&cookie), json!({"url": "http://127.0.0.1/admin"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_json(resp).await["error"].is_string());
}

#[tokio::test]
async fn text_paragraphs_flatten_to_timeline() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "text@example.com").await;

    let book = create_paste_book(
        &app,
        &cookie,
        Some("Flatten"),
        "First sentence here.\n\nSecond paragraph, longer, with more words to read.\n\nThird.",
    )
    .await;
    let id = book["id"].as_str().expect("id").to_string();

    let text = body_json(send(&app, bare_request("GET", &format!("/api/books/{id}/text"), Some(&cookie))).await).await;
    let flat: Vec<String> = text["paragraphs"]
        .as_array()
        .expect("paragraphs")
        .iter()
        .flat_map(|p| p.as_array().expect("para").iter().map(|w| w.as_str().expect("word").to_string()))
        .collect();

    let timeline = body_json(send(&app, bare_request("GET", &format!("/api/books/{id}/timeline"), Some(&cookie))).await).await;
    let tl_words: Vec<String> = timeline["words"]
        .as_array()
        .expect("words")
        .iter()
        .map(|w| w[0].as_str().expect("text").to_string())
        .collect();

    assert_eq!(flat, tl_words, "flattened text must equal the timeline word order");
    assert_eq!(flat.len() as i64, book["word_count"].as_i64().expect("count"));
}

#[tokio::test]
async fn search_scopes_to_user_and_matches_title_and_body() {
    let (app, _dir) = test_app();
    let alice = register(&app, "searcha@example.com").await;
    let bob = register(&app, "searchb@example.com").await;

    create_paste_book(&app, &alice, Some("Astronomy Notes"), "The telescope revealed distant nebulae.").await;
    create_paste_book(&app, &alice, Some("Cooking"), "A recipe for sourdough bread.").await;
    create_paste_book(&app, &bob, Some("Astronomy Secrets"), "Bob's private telescope notes.").await;

    // Match by title word.
    let resp = send(&app, bare_request("GET", "/api/books?q=astronomy", Some(&alice))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let hits = body_json(resp).await;
    let hits = hits.as_array().expect("array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["title"], "Astronomy Notes");

    // Match by body word, still scoped to Alice (Bob's telescope book excluded).
    let resp = send(&app, bare_request("GET", "/api/books?q=telescope", Some(&alice))).await;
    let hits = body_json(resp).await;
    let hits = hits.as_array().expect("array");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["title"], "Astronomy Notes");

    // No q → the whole library (Astronomy + Cooking + seeded intro).
    let resp = send(&app, bare_request("GET", "/api/books", Some(&alice))).await;
    assert_eq!(body_json(resp).await.as_array().map(Vec::len), Some(3));

    // A no-op / punctuation-only query returns an empty list, never a 500.
    let resp = send(&app, bare_request("GET", "/api/books?q=%20%2A%2A%2A", Some(&alice))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await.as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn integrations_null_and_configured() {
    // Unconfigured: both integrations are null.
    let (app, _dir) = test_app();
    let resp = send(&app, bare_request("GET", "/api/integrations", None)).await;
    assert_eq!(resp.status(), StatusCode::OK); // public, no auth
    let body = body_json(resp).await;
    assert_eq!(body, json!({"dropbox": Value::Null, "google_picker": Value::Null}));

    // Dropbox key alone lights up Dropbox; Google needs BOTH client id + api key.
    let (app, _dir) = test_app_with_config(|c| {
        c.dropbox_app_key = Some("dbx-key-123".into());
        c.oauth_google = Some(OAuthCreds {
            client_id: "goog-client".into(),
            client_secret: "goog-secret".into(),
        });
        c.google_picker_api_key = Some("picker-api-key".into());
    });
    let body = body_json(send(&app, bare_request("GET", "/api/integrations", None)).await).await;
    assert_eq!(body["dropbox"], json!({"app_key": "dbx-key-123"}));
    assert_eq!(body["google_picker"], json!({"client_id": "goog-client", "api_key": "picker-api-key"}));

    // Google client id present but no picker api key → google_picker stays null.
    let (app, _dir) = test_app_with_config(|c| {
        c.oauth_google = Some(OAuthCreds {
            client_id: "goog-client".into(),
            client_secret: "goog-secret".into(),
        });
    });
    let body = body_json(send(&app, bare_request("GET", "/api/integrations", None)).await).await;
    assert_eq!(body["google_picker"], Value::Null);
    assert_eq!(body["dropbox"], Value::Null);
}
