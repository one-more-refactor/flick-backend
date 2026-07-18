//! Server configuration from environment variables (docs/CONTRACTS.md).

use std::path::PathBuf;

/// Which product edition this server runs (CONTRACTS.md "Editions & plans"):
/// `selfhost` (the default — everything free, nothing enforced) or `hosted`
/// (the free/pro plans apply). Clients switch Pro/Contribute UI on
/// `GET /api/meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Selfhost,
    Hosted,
}

impl Edition {
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::Selfhost => "selfhost",
            Edition::Hosted => "hosted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
}

/// Client credentials for a fixed-issuer OAuth provider (Google/GitHub).
#[derive(Debug, Clone)]
pub struct OAuthCreds {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub edition: Edition,
    pub addr: String,
    pub data_dir: PathBuf,
    pub public_url: String,
    pub web_dist: PathBuf,
    pub oidc: Option<OidcSettings>,
    pub oidc_name: String,
    pub oauth_google: Option<OAuthCreds>,
    pub oauth_github: Option<OAuthCreds>,
    pub smtp_url: Option<String>,
    pub smtp_from: String,
    pub dropbox_app_key: Option<String>,
    pub google_picker_api_key: Option<String>,
    /// Bearer token guarding /api/admin/* (events). None disables admin API.
    pub admin_token: Option<String>,
}

/// Default web dist (CONTRACTS.md): first of `./web/dist`, `../web/dist`
/// containing an `index.html`, so the server finds the client whether it is
/// launched from the repo root (`cargo run -p flick-server`) or from
/// `server/`. Falls back to `./web/dist` so the "not found" notice names a
/// sensible path.
fn default_web_dist() -> PathBuf {
    let candidates = ["./web/dist", "../web/dist"];
    for dir in candidates {
        let dir = PathBuf::from(dir);
        if dir.join("index.html").is_file() {
            return dir;
        }
    }
    candidates[0].into()
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Client id + secret pair, present only when both env vars are set.
fn oauth_creds(prefix: &str) -> Option<OAuthCreds> {
    match (
        env_var(&format!("{prefix}_CLIENT_ID")),
        env_var(&format!("{prefix}_CLIENT_SECRET")),
    ) {
        (Some(client_id), Some(client_secret)) => Some(OAuthCreds {
            client_id,
            client_secret,
        }),
        _ => None,
    }
}

impl Config {
    pub fn from_env() -> Self {
        let oidc = match (
            env_var("FLICK_OIDC_ISSUER"),
            env_var("FLICK_OIDC_CLIENT_ID"),
            env_var("FLICK_OIDC_CLIENT_SECRET"),
        ) {
            (Some(issuer), Some(client_id), Some(client_secret)) => Some(OidcSettings {
                issuer,
                client_id,
                client_secret,
            }),
            _ => None,
        };
        let edition = match env_var("FLICK_EDITION").as_deref() {
            None | Some("selfhost") => Edition::Selfhost,
            Some("hosted") => Edition::Hosted,
            Some(other) => {
                tracing::warn!(
                    value = %other,
                    "unknown FLICK_EDITION (expected \"selfhost\" or \"hosted\") — using selfhost"
                );
                Edition::Selfhost
            }
        };
        Config {
            edition,
            addr: env_var("FLICK_ADDR").unwrap_or_else(|| "0.0.0.0:8484".into()),
            data_dir: env_var("FLICK_DATA_DIR")
                .unwrap_or_else(|| "./data".into())
                .into(),
            public_url: env_var("FLICK_PUBLIC_URL")
                .unwrap_or_else(|| "http://localhost:8484".into())
                .trim_end_matches('/')
                .to_string(),
            web_dist: env_var("FLICK_WEB_DIST")
                .map(PathBuf::from)
                .unwrap_or_else(default_web_dist),
            oidc,
            oidc_name: env_var("FLICK_OIDC_NAME").unwrap_or_else(|| "SSO".into()),
            oauth_google: oauth_creds("FLICK_OAUTH_GOOGLE"),
            oauth_github: oauth_creds("FLICK_OAUTH_GITHUB"),
            smtp_url: env_var("FLICK_SMTP_URL"),
            smtp_from: env_var("FLICK_SMTP_FROM")
                .unwrap_or_else(|| "flick <no-reply@localhost>".into()),
            dropbox_app_key: env_var("FLICK_DROPBOX_APP_KEY"),
            google_picker_api_key: env_var("FLICK_GOOGLE_PICKER_API_KEY"),
            admin_token: env_var("FLICK_ADMIN_TOKEN").filter(|t| !t.is_empty()),
        }
    }

    /// `Secure` cookie attribute when the public URL is https (TLS in front).
    pub fn cookie_secure(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}
