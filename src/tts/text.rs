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
    let cleaned: String = expand_comparisons(&cleaned)
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

/// Spell out `<` / `>` used as comparisons ("a < b", "3>2"): only when both
/// neighbours are spaces or digits, so markup like `<b>` or arrows (`->`) are
/// and quote markers are still dropped by `normalize`.
fn expand_comparisons(text: &str) -> String {
    if !text.contains(['<', '>']) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        // Not newlines: a `>` at the start of a line is a quote marker.
        let operand = |o: Option<&char>| o.is_some_and(|&c| c == ' ' || c.is_ascii_digit());
        let is_comparison = i > 0 && operand(chars.get(i - 1)) && operand(chars.get(i + 1));
        match c {
            '<' if is_comparison => out.push_str(" less than "),
            '>' if is_comparison => out.push_str(" greater than "),
            c => out.push(c),
        }
    }
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
    let word = word.trim_start_matches(|c: char| !c.is_alphanumeric());
    // Single initials ("J. R. R. Tolkien"), but not the pronoun "I" or a lowercase letter
    // ("so am I. Then", "solve for x. Next").
    let mut letters = word.chars();
    let initial = matches!((letters.next(), letters.next()), (Some(c), None) if c.is_uppercase() && c != 'I');
    initial || ABBREVIATIONS.contains(&word.to_lowercase().as_str())
}

/// Where to cut `chars` so the head is at most `limit` chars, preferring
/// clause punctuation, then whitespace. Only looks at the first `limit + 1`
/// chars, so walking a long text with it stays linear.
fn cut_index(chars: &[char], limit: usize) -> usize {
    if chars.len() <= limit {
        return chars.len();
    }
    let window = &chars[..limit];
    let min = limit / 3;
    window
        .iter()
        .rposition(|&c| is_clause_punct(c))
        .filter(|&p| p + 1 >= min)
        .map(|p| p + 1)
        .or_else(|| window.iter().rposition(|c| c.is_whitespace()).filter(|&p| p >= min))
        .unwrap_or(limit)
}

/// Split `s` into (head, rest) with `head` at most `limit` chars, preferring
/// clause punctuation, then whitespace.
pub fn split_at_limit(s: &str, limit: usize) -> (String, String) {
    let chars: Vec<char> = s.chars().collect();
    let cut = cut_index(&chars, limit);
    let head: String = chars[..cut].iter().collect();
    let rest: String = chars[cut..].iter().collect();
    (head.trim().to_owned(), rest.trim().to_owned())
}

pub fn pause_after(text: &str) -> u32 {
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
    let text = cap_input(text);
    let sentences: Vec<Vec<char>> = split_sentences(&normalize(text)).iter().map(|s| s.chars().collect()).collect();
    let mut chunks: Vec<Chunk> = Vec::new();

    // Walk the sentences once: (`si`, `pos`) is the start of the text not yet chunked.
    let (mut si, mut pos) = (0, 0);
    while si < sentences.len() {
        let limit = if streaming { RAMP[chunks.len().min(RAMP.len() - 1)] } else { NON_STREAMING_LIMIT };
        let sentence = &sentences[si][pos..];
        let cut = cut_index(sentence, limit);
        let mut current = String::from_iter(&sentence[..cut]).trim().to_owned();
        let rest = sentence[cut..].iter().position(|c| !c.is_whitespace());
        if let Some(skip) = rest {
            pos += cut + skip;
        } else {
            (si, pos) = (si + 1, 0);
            if !(streaming && chunks.is_empty()) {
                // Merge following short sentences while they fit.
                let mut len = current.chars().count();
                while let Some(next) = sentences.get(si) {
                    if len + 1 + next.len() > limit {
                        break;
                    }
                    current.push(' ');
                    current.extend(next);
                    len += 1 + next.len();
                    si += 1;
                }
            }
        }
        if current.chars().any(char::is_alphanumeric) {
            let pause_ms = pause_after(&current);
            chunks.push(Chunk { text: current, pause_ms });
        }
    }
    chunks
}

/// Longest input spoken in one request (bytes); anything beyond is dropped.
const MAX_INPUT_BYTES: usize = 1 << 20;

fn cap_input(text: &str) -> &str {
    if text.len() <= MAX_INPUT_BYTES {
        return text;
    }
    let mut end = MAX_INPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    log::warn!("text is {} bytes long; speaking only the first {end}", text.len());
    &text[..end]
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

/// `.`, `,` and `:` inside numbers, versions, times and addresses ("3.14",
/// "$1,299", "10:30", "192.168.0.1"), and the closing dot of a dotted
/// abbreviation in mid-sentence ("e.g. this"), belong to the word run so
/// espeak reads them in context instead of the model pausing on them.
fn is_inner_punct(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    if !matches!(c, '.' | ',' | ':') || i == 0 || !chars[i - 1].is_alphanumeric() {
        return false;
    }
    if chars.get(i + 1).is_some_and(|c| c.is_alphanumeric()) {
        return true;
    }
    if c != '.' {
        return false;
    }
    let word_start = chars[..i].iter().rposition(|c| c.is_whitespace()).map_or(0, |p| p + 1);
    let dotted = chars[word_start..i].contains(&'.');
    let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
    dotted && next.is_some_and(|&c| !c.is_uppercase() && !is_terminal(c))
}

/// Make dashes speakable: "-5" -> "minus 5", "10-20" / "10–20" -> "10 to 20";
/// any other dash (hyphenated words, dates, phone numbers) becomes a space.
fn expand_dashes(text: &str) -> String {
    if !text.contains(['-', '–', '−']) {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if !matches!(c, '-' | '–' | '−') {
            out.push(c);
            continue;
        }
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next_digit = chars.get(i + 1).is_some_and(char::is_ascii_digit);
        let starts_number = prev.is_none_or(|p| p.is_whitespace() || matches!(p, '(' | '[' | '"' | '“'));
        if next_digit && starts_number && c != '–' {
            out.push_str("minus ");
        } else if next_digit && prev.is_some_and(|p| p.is_ascii_digit()) && c != '−' {
            // A range, unless the word has more dashes ("2024-01-15", "1-800-555-0199").
            let start = chars[..i].iter().rposition(|c| c.is_whitespace()).map_or(0, |p| p + 1);
            let end = chars[i..].iter().position(|c| c.is_whitespace()).map_or(chars.len(), |p| i + p);
            let dashes = chars[start..end].iter().filter(|&&c| matches!(c, '-' | '–')).count();
            out.push_str(if dashes == 1 { " to " } else { " " });
        } else {
            out.push(' ');
        }
    }
    out
}

#[derive(Debug, PartialEq)]
enum Segment {
    /// Text phonemized by espeak.
    Words(String),
    /// Punctuation passed to the model as is (pauses and intonation).
    Punct(char),
}

/// Split text into word runs and the punctuation marks kept between them.
fn segments(text: &str) -> Vec<Segment> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<Segment>| {
        let t = run.trim();
        if !t.is_empty() {
            out.push(Segment::Words(t.to_owned()));
        }
        run.clear();
    };
    for (i, &c) in chars.iter().enumerate() {
        if is_kept_punct(c) && !is_inner_punct(&chars, i) {
            flush(&mut run, &mut out);
            out.push(Segment::Punct(c));
        } else {
            run.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Convert text to a Kokoro phoneme string. Punctuation is preserved (the
/// model turns it into pauses and intonation) by phonemizing the word runs
/// between punctuation marks separately.
pub fn phonemize(text: &str, lang: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    for segment in segments(&expand_dashes(text)) {
        match segment {
            Segment::Words(t) => {
                if t.chars().any(char::is_alphanumeric) {
                    let parts = espeak_rs::text_to_phonemes(&t, lang, None).map_err(|e| anyhow::anyhow!("{e}"))?;
                    out.push_str(&parts.join(" "));
                }
            }
            Segment::Punct(c) => {
                out.push(c);
                out.push(' ');
            }
        }
    }

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

/// Kokoro tokens for a phoneme string. Not truncated: callers must keep the
/// result within `MAX_TOKENS` (see `tts::Worker::chunk_tokens`).
pub fn tokenize(phonemes: &str) -> Vec<i64> {
    let v = vocab();
    phonemes.chars().filter_map(|c| v.get(&c).copied()).collect()
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

    fn words(segs: &[Segment]) -> Vec<&str> {
        segs.iter()
            .filter_map(|s| match s {
                Segment::Words(w) => Some(w.as_str()),
                Segment::Punct(_) => None,
            })
            .collect()
    }

    #[test]
    fn numbers_and_abbreviations_stay_in_one_run() {
        for t in ["3.14", "$1,299", "v1.2.3", "10:30", "192.168.0.1"] {
            assert_eq!(segments(t), vec![Segment::Words(t.to_owned())], "{t}");
        }
        let s = segments("Pi is 3.14, roughly. Version v1.2.3: done.");
        assert_eq!(words(&s), ["Pi is 3.14", "roughly", "Version v1.2.3", "done"]);
        assert_eq!(s.iter().filter(|s| matches!(s, Segment::Punct(_))).count(), 4, "{s:?}");
        // The dot of a dotted abbreviation stays mid-sentence, not at a sentence end.
        assert_eq!(words(&segments("Fruit, e.g. apples")), ["Fruit", "e.g. apples"]);
        assert_eq!(words(&segments("Fruit, e.g., apples")), ["Fruit", "e.g.", "apples"]);
        assert_eq!(words(&segments("in the U.S. Then")), ["in the U.S", "Then"]);
        // Ordinary punctuation still splits.
        assert_eq!(words(&segments("Hello,world. Bye: now")), ["Hello,world", "Bye", "now"]);
        assert_eq!(words(&segments("one, two. three")), ["one", "two", "three"]);
    }

    #[test]
    fn dashes_and_comparisons() {
        assert_eq!(expand_dashes("-5 degrees"), "minus 5 degrees");
        assert_eq!(expand_dashes("from (-3)"), "from (minus 3)");
        assert_eq!(expand_dashes("pages 10-20"), "pages 10 to 20");
        assert_eq!(expand_dashes("1990–1995"), "1990 to 1995");
        assert_eq!(expand_dashes("well-known x - y"), "well known x   y");
        assert_eq!(expand_dashes("on 2024-01-15"), "on 2024 01 15");
        assert_eq!(expand_comparisons("if a < b and 3>2"), "if a  less than  b and 3 greater than 2");
        assert_eq!(expand_comparisons("<b>bold</b> -> x\n> quote"), "<b>bold</b> -> x\n> quote");
    }

    #[test]
    fn pronoun_i_ends_a_sentence() {
        let s = split_sentences("So am I. Then we left. Solve for x. Next step.");
        assert_eq!(s.len(), 4, "{s:?}");
    }

    #[test]
    fn long_single_line_is_chunked_linearly() {
        let text = "word ".repeat(200_000);
        let t = std::time::Instant::now();
        let chunks = plan_chunks(&text, true);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(chunks.iter().all(|c| c.text.chars().count() <= RAMP[3]));
        let words: usize = chunks.iter().map(|c| c.text.split_whitespace().count()).sum();
        assert_eq!(words, 200_000);
        // No spaces at all: hard cuts at the limit.
        let chunks = plan_chunks(&"x".repeat(1000), false);
        assert_eq!(chunks.iter().map(|c| c.text.matches('x').count()).sum::<usize>(), 1000);
        assert!(chunks.iter().all(|c| c.text.len() <= NON_STREAMING_LIMIT));
    }

    /// espeak-ng is not thread-safe: keep every phonemizer check in one test.
    #[test]
    fn phonemizer_handles_us_english_and_keeps_punctuation() {
        let p = phonemize("Hello, world.", LANG).unwrap();
        assert!(p.contains(',') && p.ends_with('.'), "{p}");
        // "3.14" is read as a number, not "three. fourteen".
        let p = phonemize("It is 3.14 now.", LANG).unwrap();
        assert!(p.contains("pɔɪnt") && p.matches('.').count() == 1, "{p}");
        let p = phonemize("Pages 2-3.", LANG).unwrap();
        assert!(p.contains("tə") || p.contains("tuː θ"), "{p}");
        let p = phonemize("It was -5 outside.", LANG).unwrap();
        assert!(p.contains("mˈaɪnəs"), "{p}");
        assert!(!tokenize(&p).is_empty());
        assert!(is_supported_voice("af_heart") && is_supported_voice("am_adam"));
        assert!(!is_supported_voice("bf_emma") && !is_supported_voice("jf_alpha"));
    }
}
