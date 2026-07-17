//! Public integrations endpoint: which client-side file pickers are wired up.
//!
//! The Dropbox Chooser and Google Picker are script embeds that stay dark
//! until keys are configured (CONTRACTS.md). This endpoint is public (no auth)
//! so the homepage can decide which source buttons to show.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// GET /api/integrations — `{dropbox, google_picker}`, each `null` unless the
/// required config is present. Google needs BOTH the OAuth client id and the
/// picker API key.
pub async fn integrations(State(state): State<AppState>) -> Json<Value> {
    let dropbox = state
        .config
        .dropbox_app_key
        .as_ref()
        .map(|key| json!({ "app_key": key }));

    let google_picker = match (
        state.config.oauth_google.as_ref(),
        state.config.google_picker_api_key.as_ref(),
    ) {
        (Some(creds), Some(api_key)) => json!({
            "client_id": creds.client_id,
            "api_key": api_key,
        }),
        _ => Value::Null,
    };

    Json(json!({
        "dropbox": dropbox,
        "google_picker": google_picker,
    }))
}
