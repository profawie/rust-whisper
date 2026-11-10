//! Clean-up of Whisper output: repetition loops, duplicated lines,
//! known hallucinations, untranslated text detection, SRT writing.
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub no_speech: f32,
}

// ------------------------------------------------------------- scripts
/// Characters that should not survive a translation into English
/// (kana, CJK ideographs, hangul, fullwidth letters, and other non-Latin scripts).
pub fn is_foreign_char(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // hiragana + katakana
        | 0x31F0..=0x31FF // katakana ext
        | 0x3400..=0x4DBF // CJK ext A
        | 0x4E00..=0x9FFF // CJK unified
        | 0xF900..=0xFAFF // CJK compat
        | 0x20000..=0x2FA1F
        | 0xAC00..=0xD7AF // hangul syllables
        | 0x1100..=0x11FF | 0x3130..=0x318F // hangul jamo
        | 0xFF10..=0xFF19 | 0xFF21..=0xFF3A | 0xFF41..=0xFF5A | 0xFF66..=0xFF9F // fullwidth
        | 0x0370..=0x03FF // greek
        | 0x0400..=0x04FF // cyrillic
        | 0x0590..=0x05FF // hebrew
        | 0x0600..=0x06FF | 0x0750..=0x077F // arabic
        | 0x0900..=0x097F // devanagari
        | 0x0980..=0x09FF // bengali
        | 0x0B80..=0x0BFF // tamil
        | 0x0E00..=0x0E7F // thai
    )
}

pub fn foreign_count(text: &str) -> usize {
    text.chars().filter(|&c| is_foreign_char(c)).count()
}

// ------------------------------------------------------------- repetition
fn norm_word(w: &str) -> String {
    w.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

pub fn norm_text(t: &str) -> String {
    t.split_whitespace().map(norm_word).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Limit a run of one character to 3 ("Ahhhhhhhh" -> "Ahhh", "!!!!!!" -> "!!!").
fn squeeze_char_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = None;
    let mut run = 0;
    for c in text.chars() {
        if Some(c) == last {
            run += 1;
        } else {
            last = Some(c);
            run = 1;
        }
        if run <= 3 || c.is_whitespace() && run <= 1 {
            out.push(c);
        }
    }
    out
}

/// Remove phrase loops such as "I'm sorry. I'm sorry. I'm sorry. I'm sorry."
/// Single words may repeat twice ("No, no"), short phrases are cut when they
/// repeat 3+ times, and phrases of 4+ words are cut as soon as they repeat.
pub fn collapse_repeats(text: &str) -> String {
    let text = squeeze_char_runs(text.trim());
    // CJK text has no spaces; treat each char as a word so loops there are caught too.
    let words: Vec<&str> = text.split_whitespace().collect();
    let norm: Vec<String> = words.iter().map(|w| norm_word(w)).collect();
    let mut out: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0;
    'outer: while i < words.len() {
        for n in 1..=12.min((words.len() - i) / 2) {
            let mut count = 1;
            while i + (count + 1) * n <= words.len()
                && (0..n).all(|k| norm[i + k] == norm[i + count * n + k])
            {
                count += 1;
            }
            if norm[i..i + n].iter().all(|w| w.is_empty()) {
                continue;
            }
            let keep = match (n, count) {
                (1, c) if c >= 4 => 2,
                (2..=3, c) if c >= 3 => 1,
                (4.., c) if c >= 2 => 1,
                _ => continue,
            };
            for r in 0..keep {
                out.extend_from_slice(&words[i + r * n..i + (r + 1) * n]);
            }
            // keep the punctuation of the last repetition's final word
            {
                if let (Some(last_kept), Some(last_rep)) = (out.last_mut(), words.get(i + count * n - 1)) {
                    *last_kept = last_rep;
                }
            }
            i += count * n;
            continue 'outer;
        }
        out.push(words[i]);
        i += 1;
    }
    let joined = out.join(" ");
    collapse_cjk_runs(&joined)
}

/// Same idea for text without spaces (untranslated CJK left in the output).
fn collapse_cjk_runs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.iter().filter(|c| is_foreign_char(**c)).count() < 6 {
        return text.to_string();
    }
    let mut out = Vec::with_capacity(chars.len());
    let mut i = 0;
    'outer: while i < chars.len() {
        for n in 2..=10.min((chars.len() - i) / 2) {
            if !chars[i..i + n].iter().any(|c| is_foreign_char(*c)) {
                continue;
            }
            let mut count = 1;
            while i + (count + 1) * n <= chars.len() && chars[i..i + n] == chars[i + count * n..i + (count + 1) * n] {
                count += 1;
            }
            if count >= 3 {
                out.extend_from_slice(&chars[i..i + n]);
                i += count * n;
                continue 'outer;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out.into_iter().collect()
}

// ------------------------------------------------------------- hallucinations
const HALLUCINATIONS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you for watching this video",
    "please subscribe",
    "please subscribe to my channel",
    "please like and subscribe",
    "dont forget to like and subscribe",
    "subscribe to my channel",
    "see you in the next video",
    "see you next time",
    "thank you very much for watching",
];

/// Phrases Whisper invents from YouTube training data; never real film dialogue.
const YOUTUBE_PHRASES: &[&str] = &[
    "subscribe", "for watching", "for your viewing", "next video", "new video", "my channel", "this channel",
    "like and share", "comment below", "see you in the next",
];

/// Chatbot-style replies that Kotoba Bilingual (trained on LLM translations) sometimes
/// produces for unclear audio instead of a translation.
const CHATBOT_PHRASES: &[&str] = &[
    "provide more context", "more accurate translation", "not sure what you mean", "seems to be a typo",
    "could you please clarify", "clarify the text", "as an ai", "i cannot translate", "unable to translate",
    "the text you provided", "the sentence you provided", "dont know how to say it in english",
    "how to translate this",
];

fn is_hallucination(seg: &Segment) -> bool {
    let n = norm_text(&seg.text);
    if n.contains("amaraorg") || n.starts_with("subtitles by") || n.starts_with("translated by") || n.starts_with("subtitled by") {
        return true;
    }
    if YOUTUBE_PHRASES.iter().any(|p| n.contains(p)) && n.split(' ').count() <= 12 {
        return true;
    }
    if CHATBOT_PHRASES.iter().any(|p| n.contains(p)) {
        return true;
    }
    seg.no_speech > 0.3 && HALLUCINATIONS.iter().any(|h| n == *h)
}

// ------------------------------------------------------------- segments
/// Clean a translated segment list: loops inside a line, duplicated lines,
/// known hallucinations and overlapping timestamps.
pub fn clean_segments(mut segs: Vec<Segment>) -> Vec<Segment> {
    segs.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut out: Vec<Segment> = Vec::with_capacity(segs.len());
    for mut s in segs {
        s.text = collapse_repeats(&s.text);
        let n = norm_text(&s.text);
        if n.is_empty() || is_hallucination(&s) {
            continue;
        }
        if let Some(prev) = out.last_mut() {
            let pn = norm_text(&prev.text);
            // identical line again, or a line that only repeats the tail of the previous one
            if pn == n || (s.start - prev.end < 1.5 && n.split(' ').count() >= 3 && pn.ends_with(&n)) {
                prev.end = prev.end.max(s.end);
                continue;
            }
        }
        // A-B-A-B loops: a long line repeated within the last few lines
        if n.split(' ').count() >= 4
            && out.iter().rev().take(4).any(|p| norm_text(&p.text) == n && s.start - p.end < 10.0)
        {
            continue;
        }
        out.push(s);
    }
    fix_timing(&mut out);
    out
}

fn fix_timing(segs: &mut [Segment]) {
    for i in 0..segs.len() {
        if segs[i].end <= segs[i].start {
            segs[i].end = segs[i].start + 1.0;
        }
        if i + 1 < segs.len() && segs[i].end > segs[i + 1].start {
            segs[i].end = segs[i + 1].start.max(segs[i].start + 0.3);
        }
    }
}

/// Split long lines (several sentences) into subtitle-sized pieces,
/// sharing the segment's time by character count.
pub fn split_long(segs: Vec<Segment>, max_chars: usize) -> Vec<Segment> {
    let mut out = Vec::with_capacity(segs.len());
    for s in segs {
        if s.text.chars().count() <= max_chars {
            out.push(s);
            continue;
        }
        let pieces = split_text(&s.text, max_chars);
        let total: usize = pieces.iter().map(|p| p.chars().count()).sum::<usize>().max(1);
        let dur = s.end - s.start;
        let mut t = s.start;
        for p in pieces {
            let d = dur * p.chars().count() as f64 / total as f64;
            out.push(Segment { start: t, end: t + d, text: p, no_speech: s.no_speech });
            t += d;
        }
    }
    out
}

fn split_text(text: &str, max_chars: usize) -> Vec<String> {
    // sentence pieces first
    let mut sentences = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
        if w.ends_with(['.', '!', '?', '。', '！', '？']) {
            sentences.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        sentences.push(cur);
    }
    // then pack sentences into lines, splitting any very long sentence by words
    let mut lines: Vec<String> = Vec::new();
    for s in sentences {
        if let Some(last) = lines.last_mut() {
            if last.chars().count() + 1 + s.chars().count() <= max_chars {
                last.push(' ');
                last.push_str(&s);
                continue;
            }
        }
        if s.chars().count() <= max_chars * 3 / 2 {
            lines.push(s);
        } else {
            let mut line = String::new();
            for w in s.split_whitespace() {
                if !line.is_empty() && line.chars().count() + 1 + w.chars().count() > max_chars {
                    lines.push(std::mem::take(&mut line));
                }
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(w);
                // prefer breaking after a comma once the line is reasonably long
                if w.ends_with([',', ';', '、', '，']) && line.chars().count() >= max_chars / 2 {
                    lines.push(std::mem::take(&mut line));
                }
            }
            if !line.is_empty() {
                lines.push(line);
            }
        }
    }
    lines
}

// ------------------------------------------------------------- SRT
pub fn srt_time(seconds: f64) -> String {
    let ms = (seconds * 1000.0).round().max(0.0) as u64;
    let (h, ms) = (ms / 3_600_000, ms % 3_600_000);
    let (m, ms) = (ms / 60_000, ms % 60_000);
    let (s, ms) = (ms / 1000, ms % 1000);
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

pub fn to_srt(segs: &[Segment]) -> (String, usize) {
    let mut out = String::new();
    let mut n = 0;
    for s in segs {
        let text = s.text.trim();
        if text.is_empty() {
            continue;
        }
        let end = if s.end <= s.start { s.start + 0.5 } else { s.end };
        n += 1;
        let _ = write!(out, "{n}\n{} --> {}\n{text}\n\n", srt_time(s.start), srt_time(end));
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64, text: &str) -> Segment {
        Segment { start, end, text: text.into(), no_speech: 0.0 }
    }

    #[test]
    fn collapses_phrase_loops() {
        assert_eq!(collapse_repeats("I'm sorry. I'm sorry. I'm sorry. I'm sorry."), "I'm sorry.");
        assert_eq!(collapse_repeats("Let's go to the station. Let's go to the station."), "Let's go to the station.");
        assert_eq!(collapse_repeats("No, no, no, no, no, no!"), "No, no!");
        assert_eq!(collapse_repeats("Yes, yes. I know."), "Yes, yes. I know.");
        assert_eq!(collapse_repeats("Ahhhhhhhhh!!!!!!"), "Ahhh!!!");
        assert_eq!(collapse_repeats("ありがとうありがとうありがとうありがとう"), "ありがとう");
    }

    #[test]
    fn keeps_normal_text() {
        let t = "The cat sat on the mat and the dog sat on the rug.";
        assert_eq!(collapse_repeats(t), t);
    }

    #[test]
    fn merges_duplicate_lines() {
        let out = clean_segments(vec![
            seg(0.0, 2.0, "I will protect you."),
            seg(2.0, 4.0, "I will protect you."),
            seg(4.0, 6.0, "I will protect you."),
            seg(6.0, 8.0, "Let's go."),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].end, 6.0);
    }

    #[test]
    fn drops_abab_loops() {
        let out = clean_segments(vec![
            seg(0.0, 2.0, "Where are you going now?"),
            seg(2.0, 4.0, "I don't know at all."),
            seg(4.0, 6.0, "Where are you going now?"),
            seg(6.0, 8.0, "I don't know at all."),
        ]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn drops_chatbot_and_youtube_lines() {
        let out = clean_segments(vec![
            seg(0.0, 2.0, "Could you please provide more context or clarify the text so I can assist you?"),
            seg(2.0, 4.0, "Thank you for watching! Please subscribe!"),
            seg(4.0, 6.0, "The culprit is among us."),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "The culprit is among us.");
    }

    #[test]
    fn foreign_detection() {
        assert_eq!(foreign_count("Hello"), 0);
        assert_eq!(foreign_count("I went to 東京"), 2);
        assert_eq!(foreign_count("すごい"), 3);
    }

    #[test]
    fn splits_long_lines() {
        let s = seg(0.0, 10.0, "This is the first sentence of the line. This is the second sentence of it. And here is a third sentence that is long.");
        let out = split_long(vec![s], 60);
        assert!(out.len() >= 2);
        assert!((out.last().unwrap().end - 10.0).abs() < 1e-6);
    }

    #[test]
    fn srt_format() {
        assert_eq!(srt_time(3661.5), "01:01:01,500");
        let (s, n) = to_srt(&[seg(0.0, 1.0, "Hi")]);
        assert_eq!(n, 1);
        assert_eq!(s, "1\n00:00:00,000 --> 00:00:01,000\nHi\n\n");
    }
}
