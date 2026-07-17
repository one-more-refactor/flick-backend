//! Integration tests: the router is exercised in-process with
//! `tower::ServiceExt::oneshot` against a temp-dir SQLite database.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use flick_server::config::Config;
use flick_server::db::Db;
use flick_server::{app, AppState};

fn test_app() -> (Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        addr: "127.0.0.1:0".into(),
        data_dir: dir.path().join("data"),
        public_url: "http://localhost:8484".into(),
        web_dist: dir.path().join("no-such-dist"),
        oidc: None,
        oidc_name: "SSO".into(),
    };
    let db = Db::open(&config.data_dir).expect("open db");
    (app(AppState::new(db, config)), dir)
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
async fn non_pdf_upload_rejected() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "upload@example.com").await;

    let boundary = "XFLICKBOUNDARY";
    let body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"file\"; filename=\"notes.txt\"\r\n\
         Content-Type: text/plain\r\n\r\n\
         just some text\r\n\
         --{boundary}--\r\n"
    );
    let req = Request::builder()
        .method("POST")
        .uri("/api/books")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::COOKIE, &cookie)
        .body(Body::from(body))
        .expect("request");
    let resp = send(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"], "only PDF uploads are supported");
}

// ------------------------------------------------------------ misc routes

#[tokio::test]
async fn providers_reflects_disabled_oidc() {
    let (app, _dir) = test_app();
    let resp = send(&app, bare_request("GET", "/api/auth/providers", None)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body, json!({"oidc": {"enabled": false, "name": "SSO"}}));

    // OIDC login is 404 when unconfigured
    let resp = send(&app, bare_request("GET", "/api/auth/oidc/login", None)).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
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
    assert_eq!(&bytes[..], b"flick-server: web dist not found");
}

// ------------------------------------------------------- v0.2: profile

#[tokio::test]
async fn new_user_defaults_and_starter_book() {
    let (app, _dir) = test_app();
    let cookie = register(&app, "fresh@example.com").await;

    let me = body_json(send(&app, bare_request("GET", "/api/auth/me", Some(&cookie))).await).await;
    assert_eq!(me["onboarded"], false);
    assert_eq!(me["username"], Value::Null);
    assert_eq!(me["settings"]["wpm"], 350);
    assert_eq!(me["settings"]["theme"], "auto");

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
