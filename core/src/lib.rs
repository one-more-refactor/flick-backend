//! flick-core: the shared reading engine.
//!
//! Turns raw text into a reading timeline: for every word, the index of its
//! optimal-recognition-point (ORP) pivot letter and a relative duration
//! weight. Clients render `weight * (60000 / wpm)` ms per word and never
//! reimplement this logic. The rules are specified in docs/CONTRACTS.md;
//! this crate is their only implementation.

use serde::{Deserialize, Serialize};

pub const TIMELINE_VERSION: u32 = 1;

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

/// Split text into timeline words, paragraph-aware.
///
/// A paragraph break is one or more blank lines; the word preceding it gets
/// the paragraph dwell weight instead of the sentence weight.
fn tokenize(text: &str) -> Vec<TimelineWord> {
    let mut out = Vec::new();
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .flat_map(|p| p.split("\r\n\r\n"))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();

    for (pi, para) in paragraphs.iter().enumerate() {
        let tokens: Vec<&str> = para.split_whitespace().collect();
        let last = tokens.len().saturating_sub(1);
        for (wi, token) in tokens.iter().enumerate() {
            let ends_paragraph = wi == last && pi + 1 != paragraphs.len();
            out.push(TimelineWord(
                token.to_string(),
                orp_index(token),
                weight(token, ends_paragraph),
            ));
        }
    }
    out
}

/// Byte range of the word's alphanumeric core (skips leading/trailing
/// punctuation). Falls back to the whole token if nothing alphanumeric.
fn core_range(token: &str) -> (usize, usize) {
    let start = token.find(|c: char| c.is_alphanumeric());
    match start {
        None => (0, token.chars().count()),
        Some(_) => {
            let chars: Vec<char> = token.chars().collect();
            let s = chars.iter().position(|c| c.is_alphanumeric()).unwrap();
            let e = chars.iter().rposition(|c| c.is_alphanumeric()).unwrap();
            (s, e + 1)
        }
    }
}

/// ORP pivot index, in chars, relative to the full token (CONTRACTS.md table).
pub fn orp_index(token: &str) -> usize {
    let (start, end) = core_range(token);
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

/// Relative duration weight (CONTRACTS.md rules), rounded to 2 decimals.
pub fn weight(token: &str, ends_paragraph: bool) -> f32 {
    let (start, end) = core_range(token);
    let core_len = end - start;
    let mut w = 1.0_f32;
    if core_len > 8 {
        w *= 1.3;
    }
    // Trailing punctuation, ignoring closing quotes/brackets after it.
    let tail: String = token
        .chars()
        .rev()
        .skip_while(|c| matches!(c, '"' | '\'' | '\u{2019}' | '\u{201D}' | ')' | ']' | '}'))
        .take(1)
        .collect();
    if ends_paragraph {
        w *= 2.6;
    } else if matches!(tail.as_str(), "." | "!" | "?" | "\u{2026}") {
        w *= 2.1;
    } else if matches!(tail.as_str(), "," | ";" | ":") {
        w *= 1.6;
    }
    (w * 100.0).round() / 100.0
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
        assert_eq!(orp_index("incomprehensible"), 4); // len 16 -> 4
    }

    #[test]
    fn orp_skips_leading_punctuation() {
        assert_eq!(orp_index("(word"), 2); // core "word" len 4 -> +1, +1 offset
        assert_eq!(orp_index("\u{201C}hello,\u{201D}"), 2);
    }

    #[test]
    fn weights_match_contract() {
        assert_eq!(weight("word", false), 1.0);
        assert_eq!(weight("word,", false), 1.6);
        assert_eq!(weight("word.", false), 2.1);
        assert_eq!(weight("word.\u{201D}", false), 2.1); // closing quote after period
        assert_eq!(weight("customary", false), 1.3); // core len 9 > 8
        assert_eq!(weight("customary.", false), 2.73); // 1.3 * 2.1
        assert_eq!(weight("word.", true), 2.6); // paragraph replaces sentence
        assert_eq!(weight("customary.", true), 3.38); // 1.3 * 2.6
    }

    #[test]
    fn tokenize_paragraphs() {
        let t = Timeline::from_text("One two.\n\nThree four.");
        let texts: Vec<&str> = t.words.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(texts, ["One", "two.", "Three", "four."]);
        assert_eq!(t.words[1].2, 2.6); // ends paragraph (another follows)
        assert_eq!(t.words[3].2, 2.1); // last word of last paragraph: sentence weight
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
    fn handles_unicode_words() {
        // Umlauts count as alphanumeric; pivot indexes chars not bytes.
        assert_eq!(orp_index("f\u{00FC}r"), 1);
        let t = Timeline::from_text("Gr\u{00FC}\u{00DF}e aus M\u{00FC}nchen.");
        assert_eq!(t.word_count, 3);
    }
}
