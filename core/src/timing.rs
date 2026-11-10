//! Subtitle timing, TV / Netflix style.
//!
//! Whisper's segment times are coarse (often whole 2 s steps), and when it
//! translates, a line can stay on screen well after the speaker has stopped,
//! for example over the action that follows the dialogue. Here every cue is
//! snapped to the speech that voice detection actually heard:
//!
//! * a cue starts when the speech under it starts (not in the silence before it);
//! * it ends shortly after that speech ends, and never stays longer than the
//!   time needed to read it;
//! * short cues still stay long enough to read, without running into the next one;
//! * cues never overlap and keep a small gap, as broadcast subtitles do.
use crate::cleanup::Segment;

/// Time kept on screen after the voice stops.
pub const TAIL: f64 = 0.35;
/// Minimum time any cue stays on screen (~5/6 s, Netflix minimum is 20 frames).
pub const MIN_DURATION: f64 = 0.85;
/// Maximum time any cue stays on screen.
pub const MAX_DURATION: f64 = 7.0;
/// Reading speed (characters per second) used to cap how long a cue lingers.
pub const READING_CPS: f64 = 15.0;
/// Gap left between two cues (2 frames at 24 fps).
pub const MIN_GAP: f64 = 0.084;
/// A cue that lasts this long past the voice is treated as hanging over silence.
const HANG: f64 = 0.8;
/// Silence that separates two stretches of speech (e.g. dialogue, then action).
const SPLIT_GAP: f64 = 1.0;
/// How far Whisper's start/end may be off and still be matched to speech.
const SLACK: f64 = 0.6;

fn chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

/// Time a viewer needs to read `text`.
pub fn reading_time(text: &str) -> f64 {
    (chars(text) as f64 / READING_CPS + 0.5).clamp(MIN_DURATION, MAX_DURATION)
}

/// Re-time `segs` (sorted or not) against `speech` regions in seconds (sorted).
pub fn retime(mut segs: Vec<Segment>, speech: &[(f64, f64)]) -> Vec<Segment> {
    segs.sort_by(|a, b| a.start.total_cmp(&b.start));
    // a cue may only use the voice up to where the next cue starts
    let limits: Vec<f64> = (0..segs.len()).map(|i| segs.get(i + 1).map(|n| n.start).unwrap_or(f64::INFINITY)).collect();
    let mut out: Vec<Segment> = segs.into_iter().zip(limits).flat_map(|(s, limit)| fit(s, speech, limit)).collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    finish(&mut out);
    out
}

/// Voice islands under the cue, grouped where the silence between them is short.
fn voice_groups(s: &Segment, speech: &[(f64, f64)], limit: f64) -> Vec<(f64, f64, f64)> {
    let lo = s.start - SLACK;
    let hi = (s.end + SLACK * 0.5).min(limit.max(s.start + 0.5));
    let mut groups: Vec<(f64, f64, f64)> = Vec::new(); // (start, end, voiced seconds)
    for &(a, b) in speech.iter().filter(|&&(a, b)| a < hi && b > lo) {
        let (a, b) = (a.max(lo), b.min(hi));
        match groups.last_mut() {
            Some(g) if a - g.1 <= SPLIT_GAP => {
                g.1 = b;
                g.2 += b - a;
            }
            _ => groups.push((a, b, b - a)),
        }
    }
    groups
}

/// Split text into sentences (keeps the punctuation).
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
        let abbreviation = ["Dr.", "Mr.", "Mrs.", "Ms.", "St.", "Prof.", "Mt.", "No."].contains(&w);
        if w.ends_with(['.', '!', '?', '…']) && !abbreviation {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Fit one cue to the voice under it. A cue whose text has several sentences and
/// whose window covers separate stretches of speech (dialogue, action, dialogue)
/// is split so each part is shown only while its speech is heard.
fn fit(s: Segment, speech: &[(f64, f64)], limit: f64) -> Vec<Segment> {
    let groups = voice_groups(&s, speech, limit);
    let sents = sentences(&s.text);
    if groups.len() >= 2 && sents.len() >= 2 {
        let parts = groups.len().min(sents.len());
        // the last group takes the remaining sentences / speech
        let mut gsets: Vec<Vec<(f64, f64, f64)>> = groups.iter().map(|g| vec![*g]).collect();
        while gsets.len() > parts {
            // merge the pair of neighbours with the smallest silence between them
            let i = (0..gsets.len() - 1)
                .min_by(|&i, &j| {
                    let gi = gsets[i + 1][0].0 - gsets[i].last().unwrap().1;
                    let gj = gsets[j + 1][0].0 - gsets[j].last().unwrap().1;
                    gi.total_cmp(&gj)
                })
                .unwrap();
            let next = gsets.remove(i + 1);
            gsets[i].extend(next);
        }
        let voiced: Vec<f64> = gsets.iter().map(|g| g.iter().map(|x| x.2).sum::<f64>().max(0.1)).collect();
        let total_voiced: f64 = voiced.iter().sum();
        let total_chars: usize = sents.iter().map(|t| chars(t)).sum::<usize>().max(1);
        // give each voice group a share of the sentences proportional to its speech time
        let mut out = Vec::new();
        let mut si = 0;
        let mut acc_chars = 0usize;
        let mut acc_voice = 0.0;
        for (k, g) in gsets.iter().enumerate() {
            acc_voice += voiced[k];
            let target = acc_voice / total_voiced * total_chars as f64;
            let mut text = Vec::new();
            let remaining_groups = gsets.len() - k - 1;
            while si < sents.len() && sents.len() - si > remaining_groups {
                let c = chars(&sents[si]);
                if !text.is_empty() && (acc_chars as f64 + c as f64 / 2.0) > target && k + 1 < gsets.len() {
                    break;
                }
                text.push(sents[si].clone());
                acc_chars += c;
                si += 1;
            }
            if k + 1 == gsets.len() {
                while si < sents.len() {
                    text.push(sents[si].clone());
                    si += 1;
                }
            }
            if text.is_empty() {
                continue;
            }
            let mut part = Segment {
                start: g[0].0,
                end: g.last().unwrap().1 + TAIL,
                text: text.join(" "),
                no_speech: s.no_speech,
            };
            part.end = part.end.min(part.start + MAX_DURATION);
            out.push(part);
        }
        if !out.is_empty() {
            return out;
        }
    }
    let mut s = s;
    snap(&mut s, &groups);
    vec![s]
}

/// Fit a one-part cue: start on the voice, end shortly after it.
fn snap(s: &mut Segment, groups: &[(f64, f64, f64)]) {
    let lo = s.start - SLACK;
    let Some(first) = groups.first() else {
        // no voice information: never longer than needed to read it
        s.end = s.end.min(s.start + reading_time(&s.text)).min(s.start + MAX_DURATION);
        return;
    };
    // start: on the first speech, unless Whisper already starts later inside it
    let start = if s.start < first.0 || s.start - first.0 < SLACK { first.0.max(lo) } else { s.start };
    // A one-sentence cue belongs to one stretch of speech: the first one, unless
    // it is too short to carry the line (then the voice continues in the next).
    let need = (reading_time(&s.text) * 0.4).min(1.5);
    let mut voice_end = first.1;
    for g in groups.iter().skip(1) {
        if voice_end - start >= need {
            break;
        }
        voice_end = g.1;
    }
    let speech_end = voice_end + TAIL;
    let end = if s.end - speech_end > HANG {
        // the line hangs over silence / action after the voice stopped: cut it,
        // keeping enough time to read it unless more speech starts
        let next_voice = groups.iter().map(|g| g.0).find(|&a| a > voice_end).unwrap_or(f64::INFINITY);
        speech_end.max((start + reading_time(&s.text)).min(s.end).min(next_voice - MIN_GAP))
    } else {
        // close to the voice: keep Whisper's end, follow the voice a little if it ends early
        s.end.max(speech_end.min(s.end + SLACK))
    };
    s.start = start;
    s.end = end.max(start + 0.1).min(start + MAX_DURATION);
}

/// Minimum duration, no overlaps, small gaps closed or kept consistent.
fn finish(segs: &mut [Segment]) {
    for i in 0..segs.len() {
        let next_start = segs.get(i + 1).map(|n| n.start).unwrap_or(f64::INFINITY);
        let s = &mut segs[i];
        let want = (s.start + MIN_DURATION.max(reading_time(&s.text).min(1.5))).min(s.start + MAX_DURATION);
        if s.end < want {
            s.end = want; // short line: keep it long enough to read
        }
        if s.end > next_start - MIN_GAP {
            s.end = (next_start - MIN_GAP).max(s.start + 0.3);
        } else if next_start - s.end < 0.25 {
            s.end = next_start - MIN_GAP; // tiny flicker gap: chain the cues instead
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: f64, end: f64, text: &str) -> Segment {
        Segment { start, end, text: text.into(), no_speech: 0.0 }
    }

    #[test]
    fn ends_when_voice_stops_not_over_action() {
        // speech 10.0-11.2, Whisper keeps the line until 14.0 (action scene)
        let out = retime(vec![seg(10.0, 14.0, "It's over.")], &[(10.0, 11.2)]);
        assert!((out[0].end - (11.2 + TAIL)).abs() < 1e-6, "{:?}", out[0]);
    }

    #[test]
    fn starts_on_speech() {
        let out = retime(vec![seg(4.0, 8.0, "Where were you last night?")], &[(5.1, 7.0)]);
        assert!((out[0].start - 5.1).abs() < 1e-6);
        assert!(out[0].end <= 8.0 + 1e-6); // small overhang (< HANG) is kept
    }

    #[test]
    fn short_line_stays_readable() {
        let out = retime(vec![seg(1.0, 1.3, "Yes.")], &[(1.0, 1.3)]);
        assert!(out[0].end - out[0].start >= MIN_DURATION - 1e-6);
    }

    #[test]
    fn long_hang_capped_without_speech_data() {
        let out = retime(vec![seg(0.0, 20.0, "Thank you.")], &[]);
        assert!(out[0].end <= reading_time("Thank you.") + 1e-6);
    }

    #[test]
    fn no_overlap_and_gap() {
        let out = retime(vec![seg(0.0, 3.0, "First line here."), seg(2.0, 4.0, "Second line.")], &[(0.0, 1.9), (2.0, 3.8)]);
        assert!(out[0].end <= out[1].start - MIN_GAP + 1e-9);
    }

    #[test]
    fn splits_two_sentences_over_two_speeches() {
        // one Whisper segment covering: line A, 5 s of action, line B
        let out = retime(
            vec![seg(0.0, 10.0, "Where were you last night? I was sleeping at home.")],
            &[(0.2, 2.0), (7.0, 8.8)],
        );
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].text, "Where were you last night?");
        assert!(out[0].end < 3.0 && out[1].start >= 7.0, "{out:?}");
    }

    #[test]
    fn one_sentence_ends_with_its_speech() {
        // speech, then a burst of shouting 4 s later that belongs to the next line
        let out = retime(vec![seg(0.0, 9.0, "Is that true?")], &[(0.1, 1.4), (5.5, 6.0)]);
        assert_eq!(out.len(), 1);
        assert!(out[0].end <= 1.4 + TAIL + 0.6, "{out:?}");
    }

    #[test]
    fn slow_speech_keeps_cue() {
        // 6 s of speech for a short text: stay while the person is talking
        let out = retime(vec![seg(0.0, 6.0, "No...")], &[(0.0, 6.0)]);
        assert!(out[0].end >= 6.0);
    }
}
