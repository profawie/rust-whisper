//! Transcription (English audio) and translation-to-English (other languages)
//! with whisper.cpp, accelerated by Metal on Apple Silicon.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState, WhisperVadContext,
    WhisperVadContextParams, WhisperVadParams,
};

use crate::audio::{load_audio_for, SAMPLE_RATE};
use crate::cleanup::{clean_segments, foreign_count, split_long, to_srt, Segment};
use crate::models;
use crate::state::{human, lock, App};

/// Loaded model kept between jobs (dropped by "Clear all").
#[derive(Default)]
pub struct ModelCache {
    pub arch: String,
    pub ctx: Option<Arc<WhisperContext>>,
}

fn threads() -> i32 {
    std::env::var("THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4).min(8))
}

/// Longest piece of speech sent to Whisper in one go when translating.
/// Each piece is decoded on its own, so a repetition loop can never spread
/// past one piece (the main source of duplicated lines in long JP/ZH videos).
const MAX_CHUNK_S: f32 = 15.0;
/// Subtitle lines longer than this are split at sentence boundaries.
const MAX_LINE_CHARS: usize = 84;

pub fn run_job(app: App, path: PathBuf, filename: String, model_key: String, language: String) {
    let result = job(&app, &path, &filename, &model_key, &language);
    if let Err(e) = result {
        app.log("error", format!("Failed: {e:#}"));
        app.update(|s| {
            s.status = "error".into();
            s.stage = "Failed".into();
            s.error = format!("{e:#}");
        });
    }
    if std::fs::remove_file(&path).is_ok() {
        app.info("Removed uploaded file.");
    }
}

fn job(app: &App, path: &Path, filename: &str, model_key: &str, language: &str) -> Result<()> {
    let cfg = models::find(model_key).ok_or_else(|| anyhow!("unknown model {model_key}"))?;
    let english = language == "en";
    let arch = if english { cfg.en.clone() } else { cfg.multi.clone().ok_or_else(|| anyhow!("{} is English-only", cfg.label))? };

    // ---- models
    app.set_progress("Preparing models", 1.0);
    let vad_path = models::ensure_vad(&app.paths.models, |_, _| {}).context("downloading the VAD model")?;
    let ctx = load_model(app, &arch)?;

    // ---- audio
    app.set_progress("Reading audio", 8.0);
    app.info("Extracting audio with ffmpeg...");
    let decoded = load_audio_for(path, language)?;
    app.info(format!("Using {}.", decoded.note));
    let audio = decoded.samples;
    let secs = audio.len() / SAMPLE_RATE;
    app.info(format!("Audio length: {:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60));

    let started = Instant::now();
    let segments = if english {
        transcribe_english(app, &ctx, &audio, &vad_path)?
    } else {
        translate(app, &ctx, &audio, &vad_path, language, &arch)?
    };
    app.info(format!("Speech processed in {:.0}s.", started.elapsed().as_secs_f64()));

    // ---- timing: start/end each line on the voice, like TV subtitles
    app.set_progress("Fitting subtitle timing", 95.0);
    let segments = match fine_speech(&audio, &vad_path) {
        Ok(speech) => {
            app.info(format!("Fitting subtitle timing to {} voice regions...", speech.len()));
            crate::timing::retime(segments, &speech)
        }
        Err(e) => {
            app.warn(format!("Voice regions unavailable ({e}); using reading-time limits only."));
            crate::timing::retime(segments, &[])
        }
    };

    // ---- SRT
    app.set_progress("Writing SRT", 96.0);
    let (srt, count) = to_srt(&segments);
    if count == 0 {
        bail!("No speech detected - the SRT would be empty.");
    }
    let stem = Path::new(filename).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("subtitle");
    let out_name = format!("{stem}.srt");
    let out = app.paths.work.join(format!("{}.srt", path.file_stem().unwrap().to_string_lossy()));
    std::fs::write(&out, srt)?;
    let size = std::fs::metadata(&out)?.len();
    *lock(&app.output) = Some(out);
    app.ok(format!("Wrote {count} subtitle lines -> {out_name}"));
    app.update(|s| {
        s.status = "done".into();
        s.stage = "Done".into();
        s.progress = 100.0;
        s.output_name = out_name;
        s.output_size = size;
    });
    app.ok("Finished. Download the SRT, then press Clear all to free disk space.");
    Ok(())
}

fn load_model(app: &App, arch: &str) -> Result<Arc<WhisperContext>> {
    {
        let cache = lock(&app.model_cache);
        if cache.arch == arch {
            if let Some(ctx) = &cache.ctx {
                app.info(format!("Model '{arch}' already loaded."));
                return Ok(ctx.clone());
            }
        }
    }
    lock(&app.model_cache).ctx = None; // free the old model before loading a new one

    let file = models::ggml_path(&app.paths.models, arch);
    if !file.exists() {
        app.info(format!("Downloading model '{arch}' (first use only)..."));
        app.set_progress(&format!("Downloading model {arch}"), 1.0);
        let mut next = 10u64;
        let a2 = app.clone();
        models::ensure_model(&app.paths.models, arch, move |done, total| {
            if total == 0 {
                return;
            }
            let pct = done * 100 / total;
            a2.update(|s| s.progress = 1.0 + pct as f64 * 0.06);
            if pct >= next {
                a2.info(format!("Model download: {pct}% ({} / {})", human(done), human(total)));
                next = (pct / 10 + 1) * 10;
            }
        })?;
        app.ok(format!("Model '{arch}' downloaded."));
    }

    app.set_progress(&format!("Loading model {arch}"), 7.0);
    app.info(format!("Loading model '{arch}' (Metal GPU)..."));
    let mut p = WhisperContextParameters::default();
    p.use_gpu(true).flash_attn(true);
    let ctx = WhisperContext::new_with_params(file.to_str().unwrap(), p)
        .map_err(|e| anyhow!("could not load model {}: {e}", file.display()))?;
    let ctx = Arc::new(ctx);
    *lock(&app.model_cache) = ModelCache { arch: arch.into(), ctx: Some(ctx.clone()) };
    Ok(ctx)
}

fn base_params<'a>(threads: i32) -> FullParams<'a, 'a> {
    let mut p = FullParams::new(SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 });
    p.set_n_threads(threads);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_special(false);
    p.set_print_timestamps(false);
    p.set_no_context(true);
    p.set_suppress_blank(true);
    p.set_suppress_nst(true);
    p.set_temperature(0.0);
    p.set_temperature_inc(0.2);
    p.set_entropy_thold(2.4);
    p.set_logprob_thold(-1.0);
    p.set_no_speech_thold(0.6);
    p
}

fn collect(state: &WhisperState, offset: f64) -> Vec<Segment> {
    state
        .as_iter()
        .map(|s| Segment {
            start: offset + s.start_timestamp() as f64 / 100.0,
            end: offset + s.end_timestamp() as f64 / 100.0,
            text: s.to_str_lossy().map(|t| t.trim().to_string()).unwrap_or_default(),
            no_speech: s.no_speech_probability(),
        })
        .filter(|s| !s.text.is_empty())
        .collect()
}

// ------------------------------------------------------------------ English
/// English audio: one whisper.cpp pass with its built-in Silero VAD.
fn transcribe_english(app: &App, ctx: &WhisperContext, audio: &[f32], vad: &Path) -> Result<Vec<Segment>> {
    app.set_progress("Transcribing", 10.0);
    app.info("Transcribing...");
    let mut state = ctx.create_state()?;
    let mut p = base_params(threads());
    p.set_language(Some("en"));
    p.set_translate(false);
    p.set_vad_model_path(Some(vad.to_str().unwrap()));
    p.enable_vad(true);
    p.set_vad_params(WhisperVadParams::new());
    let a2 = app.clone();
    let mut last = -10;
    p.set_progress_callback_safe(move |pct: i32| {
        a2.update(|s| s.progress = 10.0 + pct as f64 * 0.85);
        if pct - last >= 10 {
            last = pct;
            a2.info(format!("Transcribing: {pct}%"));
        }
    });
    state.full(p, audio)?;
    let segs = collect(&state, 0.0);
    app.ok(format!("Got {} segments", segs.len()));
    Ok(split_long(segs, MAX_LINE_CHARS))
}

// ------------------------------------------------------------------ translate
/// Non-English audio -> English text.
///
/// 1. Silero VAD finds the speech; speech is grouped into pieces of at most
///    ~24 s, so silence/music (where Whisper hallucinates) is never decoded.
/// 2. Every piece is translated independently (no text context carried over),
///    which stops repetition loops from snowballing into duplicated lines.
/// 3. A piece whose English still contains Japanese/Chinese/etc. characters is
///    translated again with an English prompt and temperature sampling, and the
///    attempt with the fewest untranslated characters wins.
/// 4. Repeated phrases/lines and known hallucinations are removed.
fn translate(app: &App, ctx: &WhisperContext, audio: &[f32], vad: &Path, language: &str, arch: &str) -> Result<Vec<Segment>> {
    app.set_progress("Detecting speech", 9.0);
    app.info("Detecting speech (VAD)...");
    let chunks = speech_chunks(audio, vad)?;
    let speech: f32 = chunks.iter().map(|(a, b)| b - a).sum();
    app.info(format!("Found {:.0}s of speech in {} pieces.", speech, chunks.len()));
    if chunks.is_empty() {
        return Ok(Vec::new());
    }

    let lang = if models::is_kotoba(arch) {
        Some("en") // Kotoba Bilingual: Japanese speech -> English text is requested with language "en"
    } else if language == "auto" {
        None
    } else {
        Some(language)
    };
    let label = "Translating to English";
    app.set_progress(label, 10.0);
    app.info(format!("{label}..."));
    let mut state = ctx.create_state()?;
    let threads = threads();
    let mut all = Vec::new();
    let mut retried = 0;
    let mut still_bad = 0;
    let mut last_logged = -10.0;
    for (i, &(t0, t1)) in chunks.iter().enumerate() {
        let a = ((t0 * SAMPLE_RATE as f32) as usize).min(audio.len());
        let b = ((t1 * SAMPLE_RATE as f32) as usize).min(audio.len());
        if b <= a + SAMPLE_RATE / 10 {
            continue;
        }
        let piece = &audio[a..b];
        let speech_s = (t1 - t0).max(0.1);
        let mut best = run_piece(&mut state, threads, lang, piece, t0 as f64, Attempt::Normal)?;
        let mut best_score = score(&best, speech_s);
        if best_score.needs_retry() {
            retried += 1;
            for attempt in [Attempt::Prompted, Attempt::Sampled] {
                let cand = run_piece(&mut state, threads, lang, piece, t0 as f64, attempt)?;
                let s = score(&cand, speech_s);
                if s.better_than(&best_score) {
                    best = cand;
                    best_score = s;
                }
                if !best_score.needs_retry() {
                    break;
                }
            }
            if best_score.needs_retry() {
                still_bad += 1;
            }
        }
        all.extend(best);

        let pct = (i + 1) as f64 * 100.0 / chunks.len() as f64;
        app.update(|s| s.progress = 10.0 + pct * 0.85);
        if pct - last_logged >= 10.0 || i + 1 == chunks.len() {
            last_logged = pct;
            app.info(format!("{label}: {pct:.0}%"));
        }
    }
    if retried > 0 {
        app.info(format!(
            "Re-translated {retried} pieces that came back empty, too short or untranslated ({} fixed).",
            retried - still_bad
        ));
    }
    if still_bad > 0 {
        app.warn(format!("{still_bad} pieces may still miss words (often names, songs or shouting)."));
    }
    let before = all.len();
    let cleaned = clean_segments(all);
    if before > cleaned.len() {
        app.info(format!("Removed {} duplicated/hallucinated lines.", before - cleaned.len()));
    }
    app.ok(format!("Got {} segments", cleaned.len()));
    Ok(split_long(cleaned, MAX_LINE_CHARS))
}

/// How good a translation of one piece looks.
#[derive(Clone, Copy, Debug)]
struct Score {
    /// characters left in Japanese/Chinese/...
    foreign: usize,
    /// English words per second of detected speech
    words_per_s: f32,
}

/// Speech that came out with fewer English words than this per second was
/// most likely skipped by Whisper (it happens under music / with fast dialogue).
const MIN_WORDS_PER_S: f32 = 0.6;

impl Score {
    fn needs_retry(&self) -> bool {
        self.foreign > 0 || self.words_per_s < MIN_WORDS_PER_S
    }
    fn better_than(&self, other: &Score) -> bool {
        if self.foreign != other.foreign {
            return self.foreign < other.foreign;
        }
        self.words_per_s > other.words_per_s * 1.15
    }
}

fn score(segs: &[Segment], speech_s: f32) -> Score {
    // judge the text the way it will be kept: without loops and hallucinations
    let kept = clean_segments(segs.to_vec());
    let text = joined(&kept);
    Score { foreign: foreign_count(&text), words_per_s: text.split_whitespace().count() as f32 / speech_s }
}

#[derive(Clone, Copy)]
enum Attempt {
    Normal,
    Prompted,
    Sampled,
}

const ENGLISH_PROMPT: &str = "The following is the English translation of the conversation.";

fn run_piece(
    state: &mut WhisperState,
    threads: i32,
    lang: Option<&str>,
    piece: &[f32],
    offset: f64,
    attempt: Attempt,
) -> Result<Vec<Segment>> {
    let mut p = match attempt {
        Attempt::Sampled => {
            let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 5 });
            p.set_n_threads(threads);
            p.set_print_progress(false);
            p.set_print_realtime(false);
            p.set_print_special(false);
            p.set_print_timestamps(false);
            p.set_no_context(true);
            p.set_suppress_blank(true);
            p.set_suppress_nst(true);
            p.set_temperature(0.4);
            p.set_temperature_inc(0.2);
            p
        }
        _ => base_params(threads),
    };
    p.set_language(lang);
    p.set_translate(true);
    // VAD already removed the silence; don't let Whisper drop quiet or noisy speech as "no speech"
    p.set_no_speech_thold(0.95);
    if !matches!(attempt, Attempt::Normal) {
        p.set_initial_prompt(ENGLISH_PROMPT);
    }
    // pad short pieces so Whisper sees at least ~1.5 s
    let min = SAMPLE_RATE * 3 / 2;
    let segs = if piece.len() < min {
        let mut padded = piece.to_vec();
        padded.resize(min, 0.0);
        state.full(p, &padded)?;
        collect(state, offset)
    } else {
        state.full(p, piece)?;
        collect(state, offset)
    };
    // an attempt that only echoed the prompt is worthless
    Ok(segs.into_iter().filter(|s| !s.text.contains(ENGLISH_PROMPT.trim_end_matches('.'))).collect())
}

fn joined(segs: &[Segment]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// Speech regions (seconds), merged into pieces of at most MAX_CHUNK_S.
fn speech_chunks(audio: &[f32], vad: &Path) -> Result<Vec<(f32, f32)>> {
    let mut cp = WhisperVadContextParams::new();
    cp.set_n_threads(4);
    cp.set_use_gpu(false);
    let mut vctx = WhisperVadContext::new(vad.to_str().unwrap(), cp).map_err(|e| anyhow!("VAD init failed: {e}"))?;
    let mut vp = WhisperVadParams::new();
    // more sensitive than the default 0.5 so dialogue under film music is kept
    vp.set_threshold(0.35);
    vp.set_min_speech_duration(200);
    vp.set_min_silence_duration(400);
    vp.set_max_speech_duration(MAX_CHUNK_S - 2.0);
    vp.set_speech_pad(250);
    let segs = vctx.segments_from_samples(vp, audio).map_err(|e| anyhow!("VAD failed: {e}"))?;
    let dur = audio.len() as f32 / SAMPLE_RATE as f32;
    // whisper.cpp reports VAD timestamps in centiseconds
    let regions: Vec<(f32, f32)> = segs.map(|s| (s.start / 100.0, (s.end / 100.0).min(dur))).collect();
    Ok(merge_regions(&regions, MAX_CHUNK_S, 1.0))
}

/// Precise voice regions (seconds) used to time the subtitles.
fn fine_speech(audio: &[f32], vad: &Path) -> Result<Vec<(f64, f64)>> {
    let mut cp = WhisperVadContextParams::new();
    cp.set_n_threads(4);
    cp.set_use_gpu(false);
    let mut vctx = WhisperVadContext::new(vad.to_str().unwrap(), cp).map_err(|e| anyhow!("VAD init failed: {e}"))?;
    let mut vp = WhisperVadParams::new();
    vp.set_threshold(0.5);
    vp.set_min_speech_duration(150);
    vp.set_min_silence_duration(200);
    vp.set_speech_pad(80);
    let segs = vctx.segments_from_samples(vp, audio).map_err(|e| anyhow!("VAD failed: {e}"))?;
    let dur = audio.len() as f64 / SAMPLE_RATE as f64;
    Ok(segs.map(|s| (s.start as f64 / 100.0, (s.end as f64 / 100.0).min(dur))).collect())
}

/// Merge neighbouring speech regions when the gap is short and the result stays below `max`.
pub fn merge_regions(regions: &[(f32, f32)], max: f32, max_gap: f32) -> Vec<(f32, f32)> {
    let mut out: Vec<(f32, f32)> = Vec::new();
    for &(a, b) in regions {
        if b <= a {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if a - last.1 <= max_gap && b - last.0 <= max {
                last.1 = b;
                continue;
            }
        }
        // a region longer than max is cut into equal parts
        let n = ((b - a) / max).ceil().max(1.0);
        let step = (b - a) / n;
        for k in 0..n as usize {
            out.push((a + step * k as f32, a + step * (k + 1) as f32));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::merge_regions;

    #[test]
    fn merges_close_regions() {
        let r = merge_regions(&[(0.0, 2.0), (2.5, 5.0), (10.0, 12.0)], 24.0, 1.0);
        assert_eq!(r, vec![(0.0, 5.0), (10.0, 12.0)]);
    }

    #[test]
    fn splits_long_region() {
        let r = merge_regions(&[(0.0, 50.0)], 24.0, 1.0);
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|(a, b)| b - a <= 24.0));
    }
}
