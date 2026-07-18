//! flick-core: the shared reading engine (v2).
//!
//! Turns raw text into a reading timeline: for every display token, the index
//! of its optimal-recognition-point (ORP) pivot letter and a relative
//! duration weight. Clients render `weight * (60000 / wpm)` ms per token and
//! never reimplement this logic. The rules are specified in
//! docs/CONTRACTS.md ("Weight model v2"); this crate is their only
//! implementation.
//!
//! The v2 weight model is grounded in eye-movement research: graded
//! word-length effects, word-frequency effects (embedded Zipf tables,
//! OpenSubtitles-derived — see NOTICE), clause/sentence wrap-up effects, the
//! digit-reading cost, Spritz-style long-word splitting, and document-mean
//! normalization so the wpm dial is true throughput.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// Timeline JSON format version (unchanged since v1 — same shape).
pub const TIMELINE_VERSION: u32 = 1;
/// Weight-model generation (contract "Weight model v2").
pub const ENGINE_VERSION: u32 = 2;

/// Cores longer than this are split into chunks for display.
const SPLIT_ABOVE: usize = 14;
/// Maximum chunk core length when splitting.
const CHUNK_LEN: usize = 10;
/// Documents with fewer entries than this skip mean-normalization.
const NORMALIZE_MIN_WORDS: usize = 20;

/// One display step: (text, orp_index, weight). Serialized as a JSON array
/// to keep timeline payloads small.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineWord(pub String, pub usize, pub f32);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Timeline {
    pub version: u32,
    pub words: Vec<TimelineWord>,
    pub word_count: usize,
}

impl Timeline {
    pub fn from_text(text: &str) -> Self {
        let words = tokenize(text);
        let word_count = words.len();
        Timeline {
            version: TIMELINE_VERSION,
            words,
            word_count,
        }
    }
}

// ------------------------------------------------------------ language

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    En,
    De,
    /// Spanish is detected for correct language routing, but has no validated
    /// Zipf table yet — its pacing is driven by the length + structure model
    /// (neutral frequency), never by mis-reading Spanish words as "rare".
    Es,
}

const EN_MARKERS: [&str; 24] = [
    "the", "and", "of", "to", "a", "in", "is", "you", "that", "it", "he", "was", "for", "on",
    "are", "as", "with", "his", "they", "at", "be", "this", "have", "from",
];
const DE_MARKERS: [&str; 24] = [
    "der", "die", "und", "das", "ist", "ich", "nicht", "sie", "es", "ein", "er", "zu", "sich",
    "den", "auf", "mit", "dass", "wie", "im", "für", "aber", "als", "auch", "war",
];
const ES_MARKERS: [&str; 24] = [
    "de", "la", "que", "el", "en", "los", "no", "un", "por", "con", "una", "su", "para", "al",
    "lo", "como", "más", "pero", "sus", "le", "se", "ha", "muy", "las",
];

/// Which frequency table fits this document: count function-word hits over
/// the first ~500 tokens. Ties (and marker-free text) fall back to English.
fn detect_lang(tokens: &[&str]) -> Lang {
    let mut en = 0usize;
    let mut de = 0usize;
    let mut es = 0usize;
    for token in tokens.iter().take(500) {
        let lower = core_of(token).to_lowercase();
        if EN_MARKERS.contains(&lower.as_str()) {
            en += 1;
        }
        if DE_MARKERS.contains(&lower.as_str()) {
            de += 1;
        }
        if ES_MARKERS.contains(&lower.as_str()) {
            es += 1;
        }
    }
    if es > en && es > de {
        Lang::Es
    } else if de > en {
        Lang::De
    } else {
        Lang::En
    }
}

// ----------------------------------------------------------- frequency

/// Zipf tables: "word z" per line where z = zipf × 10 (top 20k per language,
/// FrequencyWords / OpenSubtitles 2018, CC-BY-SA-4.0 — attribution in NOTICE).
static FREQ_EN: LazyLock<HashMap<&'static str, u8>> =
    LazyLock::new(|| parse_freq(include_str!("../assets/freq_en.txt")));
static FREQ_DE: LazyLock<HashMap<&'static str, u8>> =
    LazyLock::new(|| parse_freq(include_str!("../assets/freq_de.txt")));

fn parse_freq(raw: &'static str) -> HashMap<&'static str, u8> {
    raw.lines()
        .filter_map(|line| {
            let (word, z) = line.split_once(' ')?;
            Some((word, z.parse::<u8>().ok()?))
        })
        .collect()
}

/// Frequency factor from the word's Zipf band (contract v2 table).
fn freq_factor(core: &str, lang: Lang) -> f32 {
    let table = match lang {
        Lang::En => &*FREQ_EN,
        Lang::De => &*FREQ_DE,
        // No validated Spanish table yet: stay neutral and let length +
        // structure carry the pacing (honest > a wrong frequency signal).
        Lang::Es => return 1.0,
    };
    let lower = core.to_lowercase();
    match table.get(lower.as_str()) {
        Some(&z10) => {
            let z = f32::from(z10) / 10.0;
            if z >= 6.0 {
                0.85
            } else if z >= 5.0 {
                0.92
            } else if z >= 4.0 {
                1.0
            } else if z >= 3.5 {
                1.12
            } else {
                1.22
            }
        }
        None => {
            // Unknown: names and rare words. Proper nouns are recognized
            // fast despite being absent from the table; very short unknowns
            // are usually interjections.
            if core.chars().count() <= 3 {
                1.0
            } else if core.chars().next().is_some_and(char::is_uppercase) {
                1.12
            } else {
                1.28
            }
        }
    }
}

// ------------------------------------------------------------ tokenizing

/// Split text into trimmed, non-empty paragraphs (blank-line separated). The
/// single source of truth for paragraph boundaries, shared by the timeline
/// tokenizer and [`paragraphs`] so their word sequences line up exactly.
fn split_paragraphs(text: &str) -> Vec<&str> {
    text.split("\n\n")
        .flat_map(|p| p.split("\r\n\r\n"))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Display tokens for one whitespace token: the token itself, or its
/// Spritz-style chunks when the alphanumeric core exceeds [`SPLIT_ABOVE`]
/// chars. All but the last chunk get a trailing `-`; leading punctuation
/// stays on the first chunk, trailing punctuation on the last.
fn display_tokens(token: &str) -> Vec<String> {
    let chars: Vec<char> = token.chars().collect();
    let (start, end) = core_bounds(&chars);
    let core_len = end - start;
    if core_len <= SPLIT_ABOVE {
        return vec![token.to_string()];
    }
    let is_vowel = |c: char| {
        matches!(
            c.to_lowercase().next().unwrap_or(c),
            'a' | 'e' | 'i' | 'o' | 'u' | 'ä' | 'ö' | 'ü' | 'y'
        )
    };
    let mut out = Vec::new();
    let mut pos = start;
    while pos < end {
        let remaining = end - pos;
        let take = if remaining <= CHUNK_LEN + (SPLIT_ABOVE - CHUNK_LEN) && remaining <= SPLIT_ABOVE
        {
            remaining
        } else {
            // Prefer to break just after a vowel near the cap (reads like a
            // hyphenation point); fall back to the hard cap.
            let mut cut = CHUNK_LEN;
            for back in 0..4 {
                let candidate = CHUNK_LEN - back;
                if candidate >= 4 && is_vowel(chars[pos + candidate - 1]) {
                    cut = candidate;
                    break;
                }
            }
            cut
        };
        let mut chunk = String::new();
        if out.is_empty() {
            chunk.extend(&chars[..start]); // leading punctuation
        }
        chunk.extend(&chars[pos..pos + take]);
        pos += take;
        if pos < end {
            chunk.push('-');
        } else {
            chunk.extend(&chars[end..]); // trailing punctuation
        }
        out.push(chunk);
    }
    out
}

/// Split text into timeline words: paragraph-aware, long words split, weight
/// model v2 applied, then document-mean normalized (contract v2).
fn tokenize(text: &str) -> Vec<TimelineWord> {
    let paragraphs = split_paragraphs(text);
    let all_tokens: Vec<&str> = paragraphs
        .iter()
        .flat_map(|p| p.split_whitespace())
        .collect();
    let lang = detect_lang(&all_tokens);

    let mut out = Vec::new();
    // Sentence length feeds the scaled wrap-up; a rare word marks the next
    // one for spillover (both stream state, reset per sentence / paragraph).
    let mut sentence_len = 0usize;
    let mut after_rare = false;
    for (pi, para) in paragraphs.iter().enumerate() {
        let tokens: Vec<&str> = para.split_whitespace().collect();
        let last = tokens.len().saturating_sub(1);
        for (wi, token) in tokens.iter().enumerate() {
            let ends_paragraph = wi == last && pi + 1 != paragraphs.len();
            sentence_len += 1;
            let chunks = display_tokens(token);
            let final_chunk = chunks.len() - 1;
            for (ci, chunk) in chunks.into_iter().enumerate() {
                // Wrap-up factor belongs to the last chunk only; frequency is
                // looked up on the full word's core (chunks are not words).
                let w = raw_weight(
                    &chunk,
                    token,
                    lang,
                    ci == final_chunk && ends_paragraph,
                    ci == final_chunk,
                    sentence_len,
                    after_rare && ci == 0,
                );
                let orp = orp_index(&chunk);
                out.push(TimelineWord(chunk, orp, w));
            }
            after_rare = freq_factor(&core_of(token), lang) >= 1.22;
            if is_sentence_final(token) || ends_paragraph {
                sentence_len = 0;
                after_rare = false;
            }
        }
    }

    // WPM honesty: normalize the document mean to 1.0 so N words at `wpm`
    // take exactly N/wpm minutes (skip for tiny snippets), then clamp+round.
    let n = out.len();
    if n >= NORMALIZE_MIN_WORDS {
        let mean: f32 = out.iter().map(|w| w.2).sum::<f32>() / n as f32;
        if mean > 0.0 {
            for w in &mut out {
                w.2 /= mean;
            }
        }
    }
    for w in &mut out {
        w.2 = (w.2.clamp(0.4, 3.6) * 100.0).round() / 100.0;
    }
    out
}

/// Word tokens grouped by paragraph, in reading order — with the SAME
/// long-word splitting as the timeline. Flattening the result yields exactly
/// the `text` fields of `Timeline::from_text(text).words`, in the same order
/// and count, so clients can map the full-text view onto timeline indices
/// 1:1 (CONTRACTS.md `GET /api/books/:id/text`).
pub fn paragraphs(text: &str) -> Vec<Vec<String>> {
    split_paragraphs(text)
        .iter()
        .map(|para| {
            para.split_whitespace()
                .flat_map(display_tokens)
                .collect()
        })
        .collect()
}

// ------------------------------------------------------------------ ORP

/// Char bounds of the token's alphanumeric core (skips leading/trailing
/// punctuation). Falls back to the whole token if nothing alphanumeric.
fn core_bounds(chars: &[char]) -> (usize, usize) {
    let s = chars.iter().position(|c| c.is_alphanumeric());
    match s {
        None => (0, chars.len()),
        Some(s) => {
            let e = chars.iter().rposition(|c| c.is_alphanumeric()).unwrap();
            (s, e + 1)
        }
    }
}

/// The token's alphanumeric core as a string.
fn core_of(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let (s, e) = core_bounds(&chars);
    chars[s..e].iter().collect()
}

/// ORP pivot index, in chars, relative to the full token (CONTRACTS.md table).
pub fn orp_index(token: &str) -> usize {
    let chars: Vec<char> = token.chars().collect();
    let (start, end) = core_bounds(&chars);
    let core_len = end - start;
    let offset = match core_len {
        0..=1 => 0,
        2..=5 => 1,
        6..=9 => 2,
        10..=13 => 3,
        _ => 4,
    };
    start + offset
}

// --------------------------------------------------------------- weights

/// The token's last meaningful punctuation char, ignoring closing
/// quotes/brackets after it.
fn tail_punct(token: &str) -> String {
    token
        .chars()
        .rev()
        .skip_while(|c| matches!(c, '"' | '\'' | '\u{2019}' | '\u{201D}' | ')' | ']' | '}'))
        .take(1)
        .collect()
}

fn is_sentence_final(token: &str) -> bool {
    matches!(tail_punct(token).as_str(), "." | "!" | "?" | "\u{2026}")
}

/// Raw (pre-normalization) weight for one display chunk. `word` is the full
/// whitespace token the chunk came from (frequency is a word property).
fn raw_weight(
    chunk: &str,
    word: &str,
    lang: Lang,
    ends_paragraph: bool,
    is_final_chunk: bool,
    sentence_len: usize,
    after_rare: bool,
) -> f32 {
    let chunk_chars: Vec<char> = chunk.chars().collect();
    let (cs, ce) = core_bounds(&chunk_chars);
    let chunk_core = &chunk_chars[cs..ce];
    let n = chunk_core.len();

    // Length: graded, no cliff (word-length effect).
    let length = 1.0 + 0.055 * (n.saturating_sub(6)) as f32;

    // Frequency: looked up on the full word's core.
    let freq = freq_factor(&core_of(word), lang);

    // Kind: digits, acronyms, internal hyphens read slower. (A split chunk's
    // trailing "-" sits outside the core, so it never triggers the hyphen
    // factor by itself.)
    let mut kind = 1.0;
    if chunk_core.iter().any(|c| c.is_ascii_digit()) {
        // Numerals draw 2.5-7x more fixations than words (PubMed 37210866).
        kind *= 1.5;
    } else if n >= 2 && chunk_core.iter().all(|c| c.is_uppercase()) {
        kind *= 1.18;
    }
    if chunk_core.contains(&'-') {
        kind *= 1.08;
    }

    // Wrap-up: paragraph > sentence > clause, never stacked; only on the
    // final chunk of a split word. Sentence wrap-up scales with sentence
    // length (Tiffin-Richards & Schroeder 2018; Spritz patent table).
    let mut wrap = 1.0;
    if is_final_chunk {
        if ends_paragraph {
            wrap = 2.8;
        } else if is_sentence_final(chunk) {
            wrap = if sentence_len > 7 { 2.3 } else { 1.9 };
        } else if matches!(tail_punct(chunk).as_str(), "," | ";" | ":" | "\u{2014}") {
            wrap = 1.5;
        }
    }

    // Spillover: a rare previous word lengthens this fixation too
    // (Rayner & Duffy 1986, ~+12 ms).
    let spill = if after_rare { 1.06 } else { 1.0 };

    (length * freq * kind * wrap * spill).clamp(0.45, 3.4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orp_follows_contract_table() {
        assert_eq!(orp_index("a"), 0); // len 1 -> 0
        assert_eq!(orp_index("to"), 1); // len 2 -> 1
        assert_eq!(orp_index("world"), 1); // len 5 -> 1
        assert_eq!(orp_index("reading"), 2); // len 7 -> 2
        assert_eq!(orp_index("comprehend"), 3); // len 10 -> 3
        assert_eq!(orp_index("incomprehensib"), 4); // len 14 -> 4
    }

    #[test]
    fn orp_skips_leading_punctuation() {
        assert_eq!(orp_index("(word"), 2); // core "word" len 4 -> +1, +1 offset
        assert_eq!(orp_index("\u{201C}hello,\u{201D}"), 2);
    }

    #[test]
    fn frequency_orders_common_before_rare() {
        // "the" (very frequent) must be faster than an obscure word of the
        // same shape; unknown long words are slowest.
        let the = freq_factor("the", Lang::En);
        let mid = freq_factor("window", Lang::En);
        let rare = freq_factor("xylographic", Lang::En);
        assert!(the < mid, "{the} < {mid}");
        assert!(mid < rare, "{mid} < {rare}");
        // German table: "und" is a top word.
        assert!(freq_factor("und", Lang::De) <= 0.85);
    }

    #[test]
    fn unknown_proper_nouns_and_short_words_stay_cheap() {
        assert_eq!(freq_factor("Qzx", Lang::En), 1.0); // short unknown
        let name = freq_factor("Bartlebooth", Lang::En); // capitalized unknown
        let rare = freq_factor("bartlebooth", Lang::En); // lowercase unknown
        assert!(name < rare);
    }

    #[test]
    fn language_detection_picks_the_right_table() {
        let en: Vec<&str> = "the cat sat on the mat and it was good"
            .split_whitespace()
            .collect();
        let de: Vec<&str> = "der Hund lief durch die Stadt und war müde"
            .split_whitespace()
            .collect();
        let es: Vec<&str> = "el perro corrió por la ciudad y no estaba muy cansado con las patas"
            .split_whitespace()
            .collect();
        assert_eq!(detect_lang(&en), Lang::En);
        assert_eq!(detect_lang(&de), Lang::De);
        assert_eq!(detect_lang(&es), Lang::Es);
        // Spanish stays neutral (no table) rather than tagging words as rare.
        assert_eq!(freq_factor("ciudad", Lang::Es), 1.0);
    }

    #[test]
    fn wrapup_ordering_holds() {
        // Same word: paragraph-final > sentence-final > clause-final > plain.
        let plain = raw_weight("word", "word", Lang::En, false, true, 5, false);
        let clause = raw_weight("word,", "word,", Lang::En, false, true, 5, false);
        let sentence = raw_weight("word.", "word.", Lang::En, false, true, 5, false);
        let para = raw_weight("word.", "word.", Lang::En, true, true, 5, false);
        assert!(plain < clause && clause < sentence && sentence < para);
        // Closing quote after the period still counts as sentence-final.
        let quoted = raw_weight("word.\u{201D}", "word.\u{201D}", Lang::En, false, true, 5, false);
        assert_eq!(quoted, sentence);
        // Long sentences earn a bigger wrap-up (scaled, Spritz-style).
        let long_sent = raw_weight("word.", "word.", Lang::En, false, true, 15, false);
        assert!(long_sent > sentence);
        // Spillover after a rare word costs a little extra.
        let spilled = raw_weight("word", "word", Lang::En, false, true, 5, true);
        assert!(spilled > plain);
    }

    #[test]
    fn digits_and_acronyms_cost_more() {
        let word = raw_weight("word", "word", Lang::En, false, true, 5, false);
        let digits = raw_weight("1984", "1984", Lang::En, false, true, 5, false);
        let acronym = raw_weight("NASA", "NASA", Lang::En, false, true, 5, false);
        assert!(digits > word);
        assert!(acronym > word);
    }

    #[test]
    fn long_words_split_into_chunks() {
        // 22-char German compound: chunks of <= CHUNK_LEN + "-".
        let chunks = display_tokens("Reisemusterkollektion.");
        assert!(chunks.len() >= 2, "{chunks:?}");
        let joined: String = chunks
            .iter()
            .map(|c| c.trim_end_matches('-'))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, "Reisemusterkollektion.");
        for c in &chunks[..chunks.len() - 1] {
            assert!(c.ends_with('-'), "{c}");
            assert!(c.trim_end_matches('-').chars().count() <= CHUNK_LEN);
        }
        assert!(chunks.last().unwrap().ends_with('.'));
        // Short words never split.
        assert_eq!(display_tokens("word."), vec!["word.".to_string()]);
        assert_eq!(display_tokens("incomprehensib"), vec!["incomprehensib".to_string()]);
    }

    #[test]
    fn document_mean_normalizes_to_one() {
        let text = "The quick brown fox jumps over the lazy dog. \
                    A journey of a thousand miles begins with a single step, \
                    they say. Extraordinarily complicated considerations \
                    notwithstanding, ordinary people read ordinary words.";
        let t = Timeline::from_text(text);
        assert!(t.word_count >= NORMALIZE_MIN_WORDS);
        let mean: f32 = t.words.iter().map(|w| w.2).sum::<f32>() / t.word_count as f32;
        assert!((mean - 1.0).abs() < 0.02, "mean {mean}");
        for w in &t.words {
            assert!((0.4..=3.6).contains(&w.2), "{w:?}");
        }
    }

    #[test]
    fn tiny_snippets_skip_normalization() {
        let t = Timeline::from_text("One two.");
        // Raw model weights survive (sentence-final ~2x a plain word).
        assert!(t.words[1].2 > t.words[0].2 * 1.5);
    }

    #[test]
    fn tokenize_paragraph_weight_beats_sentence_weight() {
        let t = Timeline::from_text("One two.\n\nThree four.");
        let texts: Vec<&str> = t.words.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(texts, ["One", "two.", "Three", "four."]);
        assert!(t.words[1].2 > t.words[3].2); // paragraph-final > sentence-final
        assert_eq!(t.word_count, 4);
    }

    #[test]
    fn timeline_word_serializes_as_array() {
        let w = TimelineWord("read,".into(), 1, 1.6);
        assert_eq!(serde_json::to_string(&w).unwrap(), r#"["read,",1,1.6]"#);
    }

    #[test]
    fn handles_empty_and_whitespace() {
        assert_eq!(Timeline::from_text("").word_count, 0);
        assert_eq!(Timeline::from_text("  \n\n  \n").word_count, 0);
    }

    #[test]
    fn paragraphs_flatten_to_timeline_words() {
        let text = "One two three.\n\nFour five with a Reisemusterkollektion.\r\n\r\nSix.";
        let paras = paragraphs(text);
        let flat: Vec<String> = paras.into_iter().flatten().collect();
        let tl_words: Vec<String> = Timeline::from_text(text)
            .words
            .iter()
            .map(|w| w.0.clone())
            .collect();
        assert_eq!(flat, tl_words); // 1:1 even with long-word splitting
    }

    #[test]
    fn handles_unicode_words() {
        // Umlauts count as alphanumeric; pivot indexes chars not bytes.
        assert_eq!(orp_index("f\u{00FC}r"), 1);
        let t = Timeline::from_text("Gr\u{00FC}\u{00DF}e aus M\u{00FC}nchen.");
        assert_eq!(t.word_count, 3);
    }
}
