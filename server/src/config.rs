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
                .unwrap_or_else(|| "../web/dist".into())
                .into(),
            oidc,
            oidc_name: env_var("FLICK_OIDC_NAME").unwrap_or_else(|| "SSO".into()),
        }
    }

    /// `Secure` cookie attribute when the public URL is https (TLS in front).
    pub fn cookie_secure(&self) -> bool {
        self.public_url.starts_with("https://")
    }
}
