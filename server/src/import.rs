//! Ingestion: turning uploads, URLs and captured HTML into book plaintext.
//!
//! The content type is decided by *bytes*, not the file extension (CONTRACTS.md
//! "Books & imports"): a `%PDF` header, an EPUB zip, Kindle `==========`
//! clippings, or plain UTF-8. Web imports additionally run readability
//! extraction (`dom_smoothie`) over fetched or supplied HTML.
//!
//! Every parser that can choke on adversarial input (pdf-extract, rbook,
//! dom_smoothie) runs inside `spawn_blocking` + `catch_unwind`, exactly like
//! the original pdf-extract path — a malformed upload is a 400, never a panic
//! that takes down the worker.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::Url;

use crate::error::AppError;

/// Fetch/import body cap (CONTRACTS.md: 25 MB), shared by URL fetch + the
/// supplied-HTML path.
pub const IMPORT_LIMIT: usize = 25 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "flick-server";

/// A book ready to be tokenized and stored: everything except the timeline.
pub struct Prepared {
    pub title: Option<String>,
    pub text: String,
    pub source: &'static str,
    pub author: Option<String>,
    pub url: Option<String>,
    pub favicon: Option<String>,
    pub excerpt: Option<String>,
    pub category: Option<String>,
}

// ---------------------------------------------------------------- sniffing

pub fn looks_like_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF")
}

/// A zip whose archive declares the EPUB mimetype. Per the OCF spec the first
/// entry is an uncompressed `mimetype` file storing `application/epub+zip`
/// verbatim near the start of the archive; sniffing for that byte string is
/// enough to route to rbook (which does the real validation).
pub fn looks_like_epub(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
        && bytes
            .windows(EPUB_MIME.len())
            .take(256)
            .any(|w| w == EPUB_MIME)
}

const EPUB_MIME: &[u8] = b"application/epub+zip";

// ------------------------------------------------------------ HTML -> text

/// Strip HTML tags to paragraph-aware plaintext: block-level elements become
/// blank-line breaks (so flick-core sees paragraphs), inline tags vanish,
/// `<script>`/`<style>` contents are dropped, and the common named/numeric
/// entities are decoded. Shared by the EPUB reader and available for any
/// XHTML-ish input.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find the tag's end.
            let Some(rel) = html[i + 1..].find('>') else {
                break;
            };
            let raw = &html[i + 1..i + 1 + rel];
            let lower = raw.trim_start_matches('/').trim().to_ascii_lowercase();
            let name: String = lower
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if matches!(name.as_str(), "script" | "style") && !raw.starts_with('/') {
                // Skip to the matching close tag.
                let close = format!("</{name}");
                if let Some(end) = html[i..].to_ascii_lowercase().find(&close) {
                    i += end;
                    // Advance past that close tag's '>'.
                    if let Some(gt) = html[i..].find('>') {
                        i += gt + 1;
                    } else {
                        break;
                    }
                    continue;
                }
                break;
            }
            if is_block_tag(&name) {
                out.push('\n');
                out.push('\n');
            }
            i += 1 + rel + 1;
        } else {
            out.push(bytes[i] as char);
            // Push the full char, not a byte, for multi-byte UTF-8.
            if bytes[i] >= 0x80 {
                out.pop();
                let ch_len = utf8_len(bytes[i]);
                if let Ok(s) = std::str::from_utf8(&bytes[i..(i + ch_len).min(bytes.len())]) {
                    out.push_str(s);
                }
                i += ch_len;
                continue;
            }
            i += 1;
        }
    }
    normalize_paragraphs(&decode_entities(&out))
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn is_block_tag(name: &str) -> bool {
    matches!(
        name,
        "p" | "br"
            | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "aside"
            | "main"
            | "blockquote"
            | "pre"
            | "li"
            | "ul"
            | "ol"
            | "dl"
            | "dd"
            | "dt"
            | "tr"
            | "table"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "hr"
            | "figure"
            | "figcaption"
    )
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let end = tail[1..].find(';').map(|e| e + 1);
        match end {
            Some(e) if e <= 12 => {
                let entity = &tail[1..e];
                let decoded = match entity {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" | "#39" => Some('\''),
                    "nbsp" => Some('\u{00A0}'),
                    "mdash" => Some('\u{2014}'),
                    "ndash" => Some('\u{2013}'),
                    "hellip" => Some('\u{2026}'),
                    "rsquo" => Some('\u{2019}'),
                    "lsquo" => Some('\u{2018}'),
                    "ldquo" => Some('\u{201C}'),
                    "rdquo" => Some('\u{201D}'),
                    num if num.starts_with("#x") || num.starts_with("#X") => {
                        u32::from_str_radix(&num[2..], 16).ok().and_then(char::from_u32)
                    }
                    num if num.starts_with('#') => {
                        num[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                match decoded {
                    Some(c) => {
                        out.push(c);
                        rest = &tail[e + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &tail[1..];
                    }
                }
            }
            _ => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse runs of blank lines to a single paragraph break and trim trailing
/// spaces on each line, so flick-core's `\n\n` paragraph split behaves.
fn normalize_paragraphs(s: &str) -> String {
    let mut paras: Vec<String> = Vec::new();
    for para in s.split("\n\n") {
        let collapsed = para.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            paras.push(collapsed);
        }
    }
    paras.join("\n\n")
}

// --------------------------------------------------------------- clippings

/// Parse a Kindle `My Clippings.txt`. Each record is separated by a line of
/// `==========`; within a record: a `Title (Author)` line, a metadata line
/// (dropped), a blank line, then the highlight text. Each highlight becomes a
/// paragraph, prefixed with its source book title when the file spans more
/// than one book (CONTRACTS.md). Returns `None` when the input has no record
/// separators (so the caller falls back to plain text).
pub fn parse_clippings(input: &str) -> Option<Prepared> {
    if !input.contains("==========") {
        return None;
    }
    struct Clip {
        book: String,
        text: String,
    }
    let mut clips: Vec<Clip> = Vec::new();
    for record in input.split("==========") {
        let record = record.trim_matches(|c: char| c == '\u{FEFF}' || c.is_whitespace());
        if record.is_empty() {
            continue;
        }
        let mut lines = record.lines();
        let Some(title_line) = lines.next() else {
            continue;
        };
        // Drop the metadata line ("- Your Highlight ...").
        let _meta = lines.next();
        let body = lines
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        if body.is_empty() {
            continue;
        }
        clips.push(Clip {
            book: clipping_book_title(title_line.trim()),
            text: body,
        });
    }
    if clips.is_empty() {
        return None;
    }

    let distinct: std::collections::BTreeSet<&str> =
        clips.iter().map(|c| c.book.as_str()).collect();
    let multi = distinct.len() > 1;
    let text = clips
        .iter()
        .map(|c| {
            if multi {
                format!("{}: {}", c.book, c.text)
            } else {
                c.text.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let (title, author) = if multi {
        ("Kindle Clippings".to_string(), None)
    } else {
        let line = clips[0].book.clone();
        // Try to peel a trailing "(Author)" from the single book's title line.
        (line, None)
    };

    Some(Prepared {
        title: Some(title),
        text,
        source: "clippings",
        author,
        url: None,
        favicon: None,
        excerpt: None,
        category: Some("docs".into()),
    })
}

/// `Book Title (Author Name)` → `Book Title` (drop the trailing author group).
fn clipping_book_title(line: &str) -> String {
    if line.ends_with(')') {
        if let Some(open) = line.rfind(" (") {
            return line[..open].trim().to_string();
        }
    }
    line.to_string()
}

// -------------------------------------------------------------------- EPUB

/// Extract EPUB plaintext + metadata via rbook, panic-guarded. Walks the spine
/// in reading order, converts each chapter's XHTML to text, and joins with
/// blank lines so paragraphs survive into the tokenizer.
pub async fn extract_epub(bytes: Vec<u8>) -> Result<Prepared, AppError> {
    let parsed = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(move || epub_to_prepared(&bytes))
    })
    .await
    .map_err(AppError::internal)?;
    match parsed {
        Ok(Some(prepared)) => Ok(prepared),
        _ => Err(AppError::bad_request(
            "could not read this EPUB file",
        )),
    }
}

fn epub_to_prepared(bytes: &[u8]) -> Option<Prepared> {
    let epub = rbook::Epub::read(std::io::Cursor::new(bytes.to_vec())).ok()?;
    let title = epub
        .metadata()
        .title()
        .map(|t| t.value().trim().to_string())
        .filter(|t| !t.is_empty());
    let author = epub
        .metadata()
        .creators()
        .next()
        .map(|c| c.value().trim().to_string())
        .filter(|a| !a.is_empty());

    let mut chapters: Vec<String> = Vec::new();
    let mut reader = epub.reader();
    while let Some(Ok(content)) = reader.read_next() {
        let text = html_to_text(content.content());
        if !text.trim().is_empty() {
            chapters.push(text);
        }
    }
    let text = chapters.join("\n\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(Prepared {
        title,
        text,
        source: "epub",
        author,
        url: None,
        favicon: None,
        excerpt: None,
        category: Some("book".into()),
    })
}

// ----------------------------------------------------- readable extraction

/// Run readability over supplied HTML for the given (absolute) page URL,
/// panic-guarded. Produces the article title, plaintext, byline and favicon.
/// `source` distinguishes the fetched (`"url"`) vs captured (`"html"`) path.
pub async fn extract_article(
    html: String,
    url: String,
    source: &'static str,
) -> Result<Prepared, AppError> {
    let page_url = url.clone();
    let article = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(move || readability(&html, &page_url))
    })
    .await
    .map_err(AppError::internal)?;
    let Ok(Some((title, text, byline, page_favicon))) = article else {
        return Err(AppError::bad_request(
            "could not extract a readable article from this page",
        ));
    };
    if text.trim().is_empty() {
        return Err(AppError::bad_request(
            "could not extract a readable article from this page",
        ));
    }
    let favicon = page_favicon.or_else(|| origin_favicon(&url));
    Ok(Prepared {
        title: (!title.trim().is_empty()).then(|| title.trim().to_string()),
        excerpt: excerpt_of(&text),
        text,
        source,
        author: byline.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()),
        url: Some(url),
        favicon,
        category: Some("article".into()),
    })
}

/// `(title, text_content, byline, page-declared favicon)` or `None` on failure.
fn readability(html: &str, url: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    let mut r = dom_smoothie::Readability::new(html, Some(url), None).ok()?;
    let article = r.parse().ok()?;
    Some((
        article.title,
        article.text_content.to_string(),
        article.byline,
        article.favicon,
    ))
}

/// `https://<origin>/favicon.ico` for a page URL (CONTRACTS.md default).
pub fn origin_favicon(url: &str) -> Option<String> {
    let parsed = Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let scheme = parsed.scheme();
    match parsed.port() {
        Some(p) => Some(format!("{scheme}://{host}:{p}/favicon.ico")),
        None => Some(format!("{scheme}://{host}/favicon.ico")),
    }
}

/// First ~30 words of the text as a one-line excerpt.
pub fn excerpt_of(text: &str) -> Option<String> {
    let words: Vec<&str> = text.split_whitespace().take(30).collect();
    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

// ---------------------------------------------------------- SSRF-safe fetch

/// Whether an IP is a public unicast address safe to fetch (CONTRACTS.md SSRF
/// guard). Rejects everything non-global: unspecified, loopback, RFC1918
/// private, CGNAT (100.64/10), link-local (169.254/16, fe80::/10), ULA
/// (fc00::/7), multicast, broadcast, documentation and reserved ranges, plus
/// any IPv4 embedded in an IPv6-mapped/compatible address.
pub fn ip_is_global(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_global(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4() {
                // ::ffff:a.b.c.d and ::a.b.c.d both carry a v4 address.
                return ipv4_is_global(&mapped);
            }
            ipv6_is_global(v6)
        }
    }
}

fn ipv4_is_global(ip: &std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
    {
        return false;
    }
    // Ranges std has no stable predicate for:
    // 100.64.0.0/10 CGNAT, 192.0.0.0/24 IETF, 198.18.0.0/15 benchmarking,
    // 240.0.0.0/4 reserved, 0.0.0.0/8 "this network".
    if a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && ip.octets()[2] == 0)
        || (a == 198 && (b == 18 || b == 19))
        || a >= 240
    {
        return false;
    }
    true
}

fn ipv6_is_global(ip: &std::net::Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    let seg0 = ip.segments()[0];
    // fc00::/7 unique-local, fe80::/10 link-local, and the 2001:db8::/32
    // documentation block.
    if (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80 || seg0 == 0x2001 && ip.segments()[1] == 0x0db8
    {
        return false;
    }
    true
}

/// Fetch a URL with the SSRF guard applied on every hop: https/http only, DNS
/// resolved to public unicast IPs, the connection pinned to the vetted IP (no
/// DNS-rebinding window), 20s timeout, ≤5 redirects, 25 MB cap. Returns the
/// final URL, the body bytes, and the `Content-Type`.
pub async fn guarded_fetch(raw_url: &str) -> Result<(String, Vec<u8>, String), AppError> {
    let mut url = Url::parse(raw_url).map_err(|_| AppError::bad_request("invalid URL"))?;

    for _ in 0..=MAX_REDIRECTS {
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(AppError::bad_request("only http and https URLs are allowed"));
        }
        let host = url
            .host_str()
            .ok_or_else(|| AppError::bad_request("URL has no host"))?
            .to_string();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| AppError::bad_request("URL has no port"))?;

        // Resolve and vet EVERY address the host maps to.
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|_| AppError::bad_request("could not resolve the URL's host"))?
            .collect();
        if addrs.is_empty() {
            return Err(AppError::bad_request("the URL's host did not resolve"));
        }
        for addr in &addrs {
            if !ip_is_global(&addr.ip()) {
                return Err(AppError::bad_request(
                    "refusing to fetch a non-public address",
                ));
            }
        }
        // Pin the connection to a vetted IP so a racing DNS change can't swap
        // in a private target between our check and reqwest's own resolution.
        let pinned = addrs[0];
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .user_agent(USER_AGENT)
            .resolve(&host, pinned)
            .build()
            .map_err(AppError::internal)?;

        let mut resp = client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| AppError::bad_request(format!("could not fetch the URL: {e}")))?;
        let status = resp.status();

        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| AppError::bad_request("redirect without a location"))?;
            url = url
                .join(location)
                .map_err(|_| AppError::bad_request("invalid redirect location"))?;
            continue;
        }
        if !status.is_success() {
            return Err(AppError::bad_request(format!(
                "the URL returned HTTP {}",
                status.as_u16()
            )));
        }

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| AppError::bad_request(format!("error reading the page: {e}")))?
        {
            if body.len() + chunk.len() > IMPORT_LIMIT {
                return Err(AppError::bad_request("the page exceeds the 25 MB limit"));
            }
            body.extend_from_slice(&chunk);
        }
        return Ok((url.to_string(), body, content_type));
    }
    Err(AppError::bad_request("too many redirects"))
}

/// Does the content look like HTML (by content type or a leading tag)?
pub fn looks_like_html(content_type: &str, bytes: &[u8]) -> bool {
    if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        return true;
    }
    let head = &bytes[..bytes.len().min(512)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    lower.contains("<!doctype html") || lower.contains("<html") || lower.contains("<body")
}
