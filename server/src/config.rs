//! Server configuration from environment variables (docs/CONTRACTS.md).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub addr: String,
    pub data_dir: PathBuf,
    pub public_url: String,
    pub web_dist: PathBuf,
    pub oidc: Option<OidcSettings>,
    pub oidc_name: String,
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
        Config {
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
        }
    }

    /// `Secure` cookie attribute when the public URL is https (TLS in front).
    pub fn cookie_secure(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}
