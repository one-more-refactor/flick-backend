//! Outbound mail (login codes) over SMTP, enabled by FLICK_SMTP_URL
//! (`smtp[s]://user:pass@host:port`, rustls only — no native TLS).

use lettre::message::header::ContentType;
use lettre::message::Mailbox;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::Config;
use crate::error::AppError;

/// Send a 6-digit login code. Only called when `config.smtp_url` is set;
/// failures are the caller's to log (the code endpoint stays a silent 204).
pub async fn send_login_code(config: &Config, to: &str, code: &str) -> Result<(), AppError> {
    let url = config
        .smtp_url
        .as_deref()
        .ok_or_else(|| AppError::Internal("send_login_code without FLICK_SMTP_URL".into()))?;
    let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
        .map_err(|e| AppError::Internal(format!("bad FLICK_SMTP_URL: {e}")))?
        .build();
    let from: Mailbox = config
        .smtp_from
        .parse()
        .map_err(|e| AppError::Internal(format!("bad FLICK_SMTP_FROM: {e}")))?;
    let to: Mailbox = to
        .parse()
        .map_err(|e| AppError::Internal(format!("bad recipient address: {e}")))?;
    let message = Message::builder()
        .from(from)
        .to(to)
        .subject(format!("{code} is your flick login code"))
        .header(ContentType::TEXT_PLAIN)
        .body(format!(
            "Your flick login code is {code}.\n\n\
             It expires in 10 minutes and can be used once.\n\
             If you didn't request it, you can ignore this mail."
        ))
        .map_err(AppError::internal)?;
    transport
        .send(message)
        .await
        .map_err(|e| AppError::Internal(format!("SMTP send failed: {e}")))?;
    Ok(())
}
