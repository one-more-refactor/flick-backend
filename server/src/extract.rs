//! Out-of-process PDF/EPUB extraction.
//!
//! pdf-extract and rbook parse untrusted uploads, and `catch_unwind` only
//! contains *panics*. It does nothing against the failure modes a hostile file
//! can trigger in those crates:
//!
//! - unbounded recursion (a Form XObject that draws itself) overflows the
//!   stack, and Rust aborts the whole process on stack overflow;
//! - decompression bombs (a FlateDecode stream or a zip entry that inflates
//!   ~1000x) allocate until the kernel OOM-kills the server;
//! - pathological inputs that keep a CPU busy for minutes.
//!
//! So each extraction runs in a short-lived child process: this same binary,
//! re-executed with [`CHILD_ARG`]. The child gets an address-space limit, the
//! parent enforces a wall-clock timeout and caps how much text it accepts,
//! and a semaphore bounds how many children run at once. A child that dies
//! for any reason is just a 400 for that one upload; the server keeps serving.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::error::AppError;

/// argv[1] that turns the server binary into an extraction child.
pub const CHILD_ARG: &str = "__flick-extract";

/// Wall-clock budget per extraction. Normal books take well under 10 s.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Address-space cap for a child. A 25 MB PDF that is a real book needs a
/// few hundred MB at most; a bomb hits this and dies instead of the host.
const CHILD_MEMORY_LIMIT: u64 = 1536 * 1024 * 1024;

/// Most extracted text accepted from a child (plus JSON overhead). Pasted
/// text is already capped at the 25 MB body limit; this keeps extracted text
/// in the same range.
pub const MAX_EXTRACTED_TEXT: usize = 32 * 1024 * 1024;

/// Children allowed to run at once. Uploads beyond this wait for a slot, so
/// a burst of hostile files can slow imports down but cannot exhaust memory.
const MAX_CONCURRENT: usize = 2;

static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(MAX_CONCURRENT));
static HELPER: OnceLock<PathBuf> = OnceLock::new();

/// Use `path` as the extraction binary instead of the current executable.
/// Integration tests run inside the test harness binary, so they point this
/// at the real `flick-server` binary. Has no effect after the first call.
pub fn set_helper(path: impl Into<PathBuf>) {
    let _ = HELPER.set(path.into());
}

fn helper() -> Result<PathBuf, AppError> {
    match HELPER.get() {
        Some(p) => Ok(p.clone()),
        None => std::env::current_exe().map_err(AppError::internal),
    }
}

/// What a child hands back.
#[derive(Debug, Serialize, Deserialize)]
pub struct Extracted {
    pub title: Option<String>,
    pub author: Option<String>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Pdf,
    Epub,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Pdf => "pdf",
            Kind::Epub => "epub",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "pdf" => Some(Kind::Pdf),
            "epub" => Some(Kind::Epub),
            _ => None,
        }
    }
}

/// Extract `bytes` in a sandboxed child. `None` = the file could not be read
/// (malformed, hostile, too big, too slow); the caller turns that into its
/// own user-facing 400.
pub async fn extract(kind: Kind, bytes: Vec<u8>) -> Result<Option<Extracted>, AppError> {
    let Ok(slot) = tokio::time::timeout(TIMEOUT, SLOTS.acquire()).await else {
        return Err(AppError::Status(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "the importer is busy right now — try again in a minute".into(),
        ));
    };
    let _slot = slot.map_err(AppError::internal)?;
    let exe = helper()?;
    let mut child = tokio::process::Command::new(exe)
        .arg(CHILD_ARG)
        .arg(kind.as_str())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Crash messages ("stack overflow", allocation failure) end up in
        // the server log next to the request that caused them.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(AppError::internal)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::internal("no child stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::internal("no child stdout"))?;

    let run = async {
        // Feed the input while reading the output, so neither pipe can stall
        // the other. A child that dies early makes the write fail; that is
        // reported through its exit status below.
        let feed = async move {
            let _ = stdin.write_all(&bytes).await;
            drop(stdin);
        };
        let read = async move {
            let mut out = Vec::new();
            let cap = (MAX_EXTRACTED_TEXT as u64) * 2 + 64 * 1024;
            let _ = stdout.take(cap + 1).read_to_end(&mut out).await;
            (out, cap)
        };
        let ((), (out, cap)) = tokio::join!(feed, read);
        let status = child.wait().await;
        (status, out, cap)
    };

    let Ok((status, out, cap)) = tokio::time::timeout(TIMEOUT, run).await else {
        // Dropping the future drops `child`, and kill_on_drop reaps it.
        tracing::warn!(kind = kind.as_str(), "extraction timed out");
        return Ok(None);
    };
    let status = status.map_err(AppError::internal)?;
    if !status.success() {
        tracing::warn!(kind = kind.as_str(), %status, "extraction failed");
        return Ok(None);
    }
    if out.len() as u64 > cap {
        return Ok(None);
    }
    // The result is the last line; anything a dependency printed to stdout
    // before it is ignored.
    let line = out
        .split(|b| *b == b'\n')
        .rev()
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    match serde_json::from_slice::<Extracted>(line) {
        Ok(e) if e.text.len() <= MAX_EXTRACTED_TEXT => Ok(Some(e)),
        _ => Ok(None),
    }
}

// ------------------------------------------------------------------ child

/// When the process was started as an extraction child, the kind to extract.
pub fn child_kind() -> Option<String> {
    let mut args = std::env::args();
    if args.nth(1).as_deref() == Some(CHILD_ARG) {
        Some(args.next().unwrap_or_default())
    } else {
        None
    }
}

/// Child entry point: stdin = file bytes, last stdout line = `Extracted`
/// JSON. Returns the process exit code. Runs before any async runtime or
/// logging is set up.
pub fn run_child(kind: &str) -> i32 {
    use std::io::{Read, Write};

    let Some(kind) = Kind::parse(kind) else {
        return 2;
    };
    limit_child_resources();

    let mut bytes = Vec::new();
    let limit = crate::books::UPLOAD_LIMIT as u64;
    if std::io::stdin()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > limit
    {
        return 1;
    }

    let extracted = std::panic::catch_unwind(move || match kind {
        Kind::Pdf => pdf_extract::extract_text_from_mem(&bytes)
            .ok()
            .map(|text| Extracted {
                title: None,
                author: None,
                text,
            }),
        Kind::Epub => crate::import::epub_to_extracted(&bytes),
    });
    let Ok(Some(mut extracted)) = extracted else {
        return 1;
    };
    // Strip control characters here already: it is needed anyway (see
    // `import::strip_controls`) and keeps the JSON escaping overhead bounded.
    extracted.text = crate::import::strip_controls(&extracted.text);
    if extracted.text.len() > MAX_EXTRACTED_TEXT {
        return 1;
    }
    let Ok(json) = serde_json::to_vec(&extracted) else {
        return 1;
    };
    let mut stdout = std::io::stdout().lock();
    if stdout.write_all(b"\n").is_err()
        || stdout.write_all(&json).is_err()
        || stdout.write_all(b"\n").is_err()
        || stdout.flush().is_err()
    {
        return 1;
    }
    0
}

/// Best effort: cap the child's address space and make it the OOM killer's
/// first choice, so a bomb takes down the child and nothing else.
fn limit_child_resources() {
    #[cfg(unix)]
    {
        let limit = libc::rlimit {
            rlim_cur: CHILD_MEMORY_LIMIT as libc::rlim_t,
            rlim_max: CHILD_MEMORY_LIMIT as libc::rlim_t,
        };
        // SAFETY: setrlimit only reads the struct we pass; failure is ignored
        // (the timeout and the semaphore still bound the damage).
        unsafe {
            libc::setrlimit(libc::RLIMIT_AS, &limit);
        }
    }
    let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
}
