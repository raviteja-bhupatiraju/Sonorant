//! Text -> chunks -> phonemes -> Kokoro tokens.
//!
//! Kokoro is not an autoregressive model: one ONNX call turns a whole token
//! sequence into audio, and cost grows with sequence length. Low latency
//! therefore comes from *chunking*: synthesize a very short first chunk so
//! audio starts almost immediately, then grow the chunk size while the
//! previous chunk is still playing (see `plan_chunks`).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::tts::vocab::VOCAB;

/// Longest token sequence the model accepts (without the two pad tokens).
pub const MAX_TOKENS: usize = 510;

/// Maximum chunk length (in characters) for the 1st, 2nd, 3rd... chunk.
/// The ramp lets synthesis of chunk N+1 finish while chunk N is playing.
const RAMP: [usize; 4] = [60, 130, 240, 320];
/// Chunk limit when streaming is disabled (still bounded by the token limit).
const NON_STREAMING_LIMIT: usize = 320;

#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    /// Silence appended after the chunk, derived from its closing punctuation.
    pub pause_ms: u32,
}

/// Sonorant speaks US English only: the espeak-ng voice used for phonemization.
pub const LANG: &str = "en-us";

/// Kokoro's US English voices are the `a*` ones (`af_*` female, `am_*` male).
pub fn is_supported_voice(voice: &str) -> bool {
    voice.starts_with('a')
}

// ---------------------------------------------------------------------------
// Normalisation and sentence splitting
// ---------------------------------------------------------------------------

fn is_terminal(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

fn is_clause_punct(c: char) -> bool {
    matches!(c, ',' | ';' | ':' | '—' | '–')
}

/// Clean up raw selected text: strip markup noise, join hard-wrapped lines and
/// turn paragraph/heading breaks into sentence breaks (`\n`).
fn normalize(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        let w = word.trim_end();
        if w.starts_with("http://") || w.starts_with("https://") || w.starts_with("www.") {
            cleaned.push_str("link");
            cleaned.push_str(&word[w.len()..]);
        } else {
            cleaned.push_str(word);
        }
    }
    let cleaned: String = cleaned
        .chars()
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' => '\'',
            '\u{00a0}' | '\t' => ' ',
            '*' | '_' | '#' | '`' | '|' | '~' | '^' | '<' | '>' | '[' | ']' | '{' | '}' | '\\' => ' ',
            c => c,
        })
        .collect();

    let mut out = String::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut String| {
        let t = cur.trim();
        if !t.is_empty() {
            out.push_str(t);
            if !t.ends_with(|c: char| is_terminal(c) || is_clause_punct(c)) {
                out.push('.');
            }
            out.push('\n');
        }
        cur.clear();
    };
    for raw in cleaned.lines() {
        let line = raw.trim();
        if line.is_empty() {
            flush(&mut cur, &mut out);
            continue;
        }
        if cur.is_empty() {
            cur.push_str(line);
        } else if cur.ends_with(|c: char| is_terminal(c) || c == ':' || c == ';') {
            flush(&mut cur, &mut out);
            cur.push_str(line);
        } else if line.starts_with(|c: char| c.is_lowercase()) || cur.ends_with(',') {
            // Hard-wrapped paragraph: the sentence continues on the next line.
            cur.push(' ');
            cur.push_str(line);
        } else {
            // Heading / list item: treat as its own sentence.
            flush(&mut cur, &mut out);
            cur.push_str(line);
        }
    }
    flush(&mut cur, &mut out);
    out
}

const ABBREVIATIONS: &[&str] = &[
    "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "vs", "etc", "e.g", "i.e", "no", "fig", "inc", "ltd", "co",
    "approx", "dept", "est",
];

fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            push_sentence(&mut sentences, &chars[start..i]);
            start = i + 1;
        } else if is_terminal(c) {
            // Consume runs like "?!" or "..." and closing quotes / brackets.
            let mut j = i + 1;
            while j < chars.len() && (is_terminal(chars[j]) || matches!(chars[j], '"' | '\'' | ')' | '”')) {
                j += 1;
            }
            let at_end = j >= chars.len();
            let followed_by_space = !at_end && chars[j].is_whitespace();
            if (at_end || followed_by_space) && !(c == '.' && j == i + 1 && is_abbreviation(&chars[start..i])) {
                push_sentence(&mut sentences, &chars[start..j]);
                start = j;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    push_sentence(&mut sentences, &chars[start..]);
    sentences
}

fn push_sentence(out: &mut Vec<String>, chars: &[char]) {
    let s = String::from_iter(chars).split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().any(char::is_alphanumeric) {
        out.push(s);
    }
}

fn is_abbreviation(before: &[char]) -> bool {
    let start = before.iter().rposition(|c| c.is_whitespace()).map_or(0, |p| p + 1);
    let word = String::from_iter(&before[start..]);
    let word = word.trim_start_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    // Single initials ("J. R. R. Tolkien") and common abbreviations.
    (word.chars().count() == 1 && word.chars().all(char::is_alphabetic)) || ABBREVIATIONS.contains(&word.as_str())
}

/// Split `s` into (head, rest) with `head` at most `limit` chars, preferring
/// clause punctuation, then whitespace.
fn split_at_limit(s: &str, limit: usize) -> (String, String) {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= limit {
        return (s.to_owned(), String::new());
    }
    let window = &chars[..limit];
    let min = limit / 3;
    let cut = window
        .iter()
        .rposition(|&c| is_clause_punct(c))
        .filter(|&p| p + 1 >= min)
        .map(|p| p + 1)
        .or_else(|| window.iter().rposition(|c| c.is_whitespace()).filter(|&p| p >= min))
        .unwrap_or(limit);
    let head: String = chars[..cut].iter().collect();
    let rest: String = chars[cut..].iter().collect();
    (head.trim().to_owned(), rest.trim().to_owned())
}

fn pause_after(text: &str) -> u32 {
    match text.trim_end_matches(['"', '\'', ')', '”']).chars().last() {
        Some(c) if is_terminal(c) => 220,
        Some(c) if is_clause_punct(c) => 110,
        _ => 0,
    }
}

/// Break text into synthesis chunks.
///
/// With `streaming`, chunk sizes ramp up (`RAMP`) so the first audio is ready
/// after synthesizing only a few words, and the first chunk is never merged
/// with following sentences.
pub fn plan_chunks(text: &str, streaming: bool) -> Vec<Chunk> {
    let mut queue: std::collections::VecDeque<String> = split_sentences(&normalize(text)).into();
    let mut chunks: Vec<Chunk> = Vec::new();

    while let Some(first) = queue.pop_front() {
        let limit = if streaming { RAMP[chunks.len().min(RAMP.len() - 1)] } else { NON_STREAMING_LIMIT };
        let (mut current, rest) = split_at_limit(&first, limit);
        if !rest.is_empty() {
            queue.push_front(rest);
        } else if !(streaming && chunks.is_empty()) {
            // Merge following short sentences while they fit.
            while let Some(next) = queue.front() {
                if current.chars().count() + 1 + next.chars().count() > limit {
                    break;
                }
                current.push(' ');
                current.push_str(next);
                queue.pop_front();
            }
        }
        if current.chars().any(char::is_alphanumeric) {
            let pause_ms = pause_after(&current);
            chunks.push(Chunk { text: current, pause_ms });
        }
    }
    chunks
}

// ---------------------------------------------------------------------------
// Phonemes and tokens
// ---------------------------------------------------------------------------

fn vocab() -> &'static HashMap<char, i64> {
    static MAP: OnceLock<HashMap<char, i64>> = OnceLock::new();
    MAP.get_or_init(|| VOCAB.iter().copied().collect())
}

fn is_kept_punct(c: char) -> bool {
    matches!(c, ';' | ':' | ',' | '.' | '!' | '?' | '—' | '…' | '"' | '(' | ')' | '“' | '”')
}

/// Convert text to a Kokoro phoneme string. Punctuation is preserved (the
/// model turns it into pauses and intonation) by phonemizing the word runs
/// between punctuation marks separately.
pub fn phonemize(text: &str, lang: &str) -> anyhow::Result<String> {
    let text = text.replace(['–', '-'], " ");
    let mut out = String::new();
    let mut run = String::new();

    let flush_run = |run: &mut String, out: &mut String| -> anyhow::Result<()> {
        let t = run.trim();
        if t.chars().any(char::is_alphanumeric) {
            let parts = espeak_rs::text_to_phonemes(t, lang, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            out.push_str(&parts.join(" "));
        }
        run.clear();
        Ok(())
    };

    for c in text.chars() {
        if is_kept_punct(c) {
            flush_run(&mut run, &mut out)?;
            out.push(c);
            out.push(' ');
        } else {
            run.push(c);
        }
    }
    flush_run(&mut run, &mut out)?;

    // Light clean-up so espeak output matches what Kokoro was trained on.
    let mapped: String = out
        .chars()
        .filter_map(|c| match c {
            '\u{361}' | '\u{303}' => None,
            'ʲ' => Some('j'),
            'r' => Some('ɹ'),
            'x' | 'ç' => Some('k'),
            'ɬ' => Some('l'),
            c => Some(c),
        })
        .filter(|c| vocab().contains_key(c))
        .collect();
    Ok(mapped.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn tokenize(phonemes: &str) -> Vec<i64> {
    let v = vocab();
    phonemes.chars().filter_map(|c| v.get(&c).copied()).take(MAX_TOKENS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_chunk_is_short_and_ramps() {
        let text = "This is the first sentence of a fairly long passage, which keeps going and going well past the first limit. \
                    Here is a second sentence. And a third one follows it closely. A fourth sentence appears too. \
                    Finally there is a fifth sentence that rounds the whole thing off nicely.";
        let chunks = plan_chunks(text, true);
        assert!(chunks.len() >= 3, "{chunks:?}");
        assert!(chunks[0].text.chars().count() <= RAMP[0], "{chunks:?}");
        assert!(chunks.iter().all(|c| c.text.chars().count() <= RAMP[3]));
        let rejoined: String = chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
        assert_eq!(rejoined.split_whitespace().count(), text.split_whitespace().count());
    }

    #[test]
    fn abbreviations_do_not_split() {
        let s = split_sentences("Dr. Smith met J. R. Tolkien at 3.14 o'clock. It was fine.");
        assert_eq!(s.len(), 2, "{s:?}");
    }

    #[test]
    fn hard_wrapped_lines_join() {
        let n = normalize("This sentence is wrapped\nacross two lines.\n\nNew paragraph");
        assert_eq!(n, "This sentence is wrapped across two lines.\nNew paragraph.\n");
    }

    /// espeak-ng is not thread-safe: keep every phonemizer check in one test.
    #[test]
    fn phonemizer_handles_us_english_and_keeps_punctuation() {
        let p = phonemize("Hello, world.", LANG).unwrap();
        assert!(p.contains(',') && p.ends_with('.'), "{p}");
        assert!(!tokenize(&p).is_empty());
        assert!(is_supported_voice("af_heart") && is_supported_voice("am_adam"));
        assert!(!is_supported_voice("bf_emma") && !is_supported_voice("jf_alpha"));
    }
}
