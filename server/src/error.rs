//! Application error type and error-shaped extractors.
//!
//! Every failure on a request path renders as `{"error": "..."}` JSON with an
//! appropriate status code — never axum's plain-text default bodies.

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized,
    NotFound,
    Conflict(String),
    /// Arbitrary status + message (body limits, upstream OIDC failures, ...).
    Status(StatusCode, String),
    /// Like `Status`, plus a machine-readable `"code"` key in the JSON body
    /// (`{"error": ..., "code": ...}`) for errors clients must branch on
    /// (e.g. the hosted weekly upload limit). Everything else stays
    /// `{"error"}`-only.
    Coded(StatusCode, String, &'static str),
    Internal(String),
}

impl AppError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::Internal(e.to_string())
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Internal(format!("database: {e}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message, code) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m, None),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "authentication required".to_string(),
                None,
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string(), None),
            Self::Conflict(m) => (StatusCode::CONFLICT, m, None),
            Self::Status(s, m) => (s, m, None),
            Self::Coded(s, m, code) => (s, m, Some(code)),
            Self::Internal(m) => {
                tracing::error!("internal error: {m}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                    None,
                )
            }
        };
        let mut body = json!({ "error": message });
        if let Some(code) = code {
            body["code"] = json!(code);
        }
        (status, Json(body)).into_response()
    }
}

/// `axum::Json` wrapper whose rejection is our JSON error shape
/// (covers malformed JSON bodies, wrong content type, over-limit bodies).
pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(rej) => {
                // axum's own text names our Rust field types and serde's
                // internals ("invalid type: integer `5`, expected a string at
                // line 1 column 10"). That is a debugging detail, not copy to
                // show a reader — clients render `error` verbatim. Keep the
                // detail in the log and answer with something a human wrote.
                tracing::debug!("json rejection: {}", rej.body_text());
                // Malformed / mismatched JSON bodies are a plain 400 for us;
                // keep 413 (too large) and 415 (wrong content type) as-is.
                let (status, message) = match &rej {
                    JsonRejection::JsonDataError(_) => (
                        StatusCode::BAD_REQUEST,
                        "the request body is missing a required field or has the wrong type",
                    ),
                    JsonRejection::JsonSyntaxError(_) => (
                        StatusCode::BAD_REQUEST,
                        "the request body is not valid JSON",
                    ),
                    JsonRejection::MissingJsonContentType(_) => (
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "expected a JSON body (content-type: application/json)",
                    ),
                    _ => match rej.status() {
                        StatusCode::PAYLOAD_TOO_LARGE => (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "the request body is too large",
                        ),
                        StatusCode::UNPROCESSABLE_ENTITY => (
                            StatusCode::BAD_REQUEST,
                            "the request body could not be read",
                        ),
                        other => (other, "the request body could not be read"),
                    },
                };
                Err(AppError::Status(status, message.into()))
            }
        }
    }
}

/// `axum::extract::Path` wrapper with the same guarantee.
pub struct AppPath<T>(pub T);

impl<S, T> FromRequestParts<S> for AppPath<T>
where
    axum::extract::Path<T>: FromRequestParts<S, Rejection = PathRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(AppPath(value)),
            Err(rej) => {
                // Same reasoning as `AppJson`: the raw text describes our route
                // signature, which is nothing the caller can act on.
                tracing::debug!("path rejection: {}", rej.body_text());
                Err(AppError::Status(
                    rej.status(),
                    "the URL is not valid for this endpoint".into(),
                ))
            }
        }
    }
}
