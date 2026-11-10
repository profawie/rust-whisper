//! Decode any video/audio file to 16 kHz mono f32 PCM with ffmpeg.
//!
//! For films the audio track matters a lot:
//! * the track in the spoken language is picked when the file has several (dub + original);
//! * 5.1 / 7.1 tracks are folded to mono *favouring the centre channel*, which carries the
//!   dialogue. A plain `-ac 1` mixes in the music and effects of all other channels at the same
//!   level, and quiet lines under music are then missed by voice detection and Whisper.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const SAMPLE_RATE: usize = 16_000;

fn tool(name: &str) -> PathBuf {
    // from PATH, or from Homebrew when PATH is minimal (apps opened from Finder)
    let in_path = std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join(name)).find(|f| f.is_file()));
    in_path
        .or_else(|| {
            ["/opt/homebrew/bin", "/usr/local/bin"].iter().map(|d| Path::new(d).join(name)).find(|p| p.is_file())
        })
        .unwrap_or_else(|| name.into())
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AudioStream {
    pub index: u32,
    #[serde(default)]
    pub channels: u32,
    #[serde(default)]
    pub channel_layout: String,
    #[serde(default)]
    pub tags: std::collections::HashMap<String, String>,
}

impl AudioStream {
    pub fn language(&self) -> &str {
        self.tags.get("language").map(String::as_str).unwrap_or("und")
    }
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<AudioStream>,
}

pub fn probe(path: &Path) -> Vec<AudioStream> {
    let out = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=index,channels,channel_layout:stream_tags=language", "-of", "json"])
        .arg(path)
        .output();
    out.ok()
        .and_then(|o| serde_json::from_slice::<Probe>(&o.stdout).ok())
        .map(|p| p.streams)
        .unwrap_or_default()
}

/// ISO 639-1 (Whisper) -> ISO 639-2 tags used in video containers.
fn container_langs(code: &str) -> &'static [&'static str] {
    match code {
        "ja" => &["jpn", "ja", "jp"],
        "zh" => &["chi", "zho", "zh", "chn", "cmn", "yue"],
        "ko" => &["kor", "ko"],
        "en" => &["eng", "en"],
        "ms" => &["msa", "may", "ms"],
        "id" => &["ind", "id"],
        "fr" => &["fra", "fre", "fr"],
        "de" => &["deu", "ger", "de"],
        "es" => &["spa", "es"],
        _ => &[],
    }
}

/// Pick the audio stream to decode: one tagged with the spoken language, else the first.
pub fn pick_stream<'a>(streams: &'a [AudioStream], language: &str) -> Option<&'a AudioStream> {
    let wanted = container_langs(language);
    streams
        .iter()
        .find(|s| wanted.iter().any(|w| s.language().eq_ignore_ascii_case(w)))
        .or_else(|| streams.first())
}

/// ffmpeg filter that folds the stream to mono, keeping dialogue on top.
pub fn mono_filter(s: &AudioStream) -> Option<String> {
    let layout = s.channel_layout.as_str();
    // layouts that have a front-centre (FC) channel
    const WITH_FC: &[&str] = &["3.0", "3.1", "4.0", "4.1", "5.0", "5.1", "6.0", "6.1", "7.0", "7.1"];
    let has_center = WITH_FC.iter().any(|l| layout.starts_with(l)) || (layout.is_empty() && s.channels >= 6);
    if has_center {
        // centre = dialogue; a little of the front left/right for off-centre voices;
        // surrounds and LFE (music, effects, rumble) are left out.
        Some("pan=mono|c0=FC+0.2*FL+0.2*FR".into())
    } else {
        None
    }
}

pub struct Decoded {
    pub samples: Vec<f32>,
    /// Human description of what was decoded, for the log.
    pub note: String,
}

pub fn load_audio(path: &Path) -> Result<Vec<f32>> {
    Ok(load_audio_for(path, "")?.samples)
}

/// Decode `path`, choosing the track in `language` and a dialogue-friendly downmix.
pub fn load_audio_for(path: &Path, language: &str) -> Result<Decoded> {
    let streams = probe(path);
    let stream = pick_stream(&streams, language);
    let mut args: Vec<String> = vec!["-nostdin".into(), "-hide_banner".into(), "-loglevel".into(), "error".into(), "-threads".into(), "0".into(), "-i".into()];
    args.push(path.to_string_lossy().into_owned());
    let mut note = String::from("audio");
    if let Some(s) = stream {
        args.extend(["-map".into(), format!("0:{}", s.index)]);
        note = format!(
            "audio track #{} ({}, {} ch{})",
            s.index,
            s.language(),
            s.channels,
            if s.channel_layout.is_empty() { String::new() } else { format!(", {}", s.channel_layout) }
        );
        if streams.len() > 1 {
            note.push_str(&format!(" of {} tracks", streams.len()));
        }
        if let Some(f) = mono_filter(s) {
            args.extend(["-af".into(), f]);
            note.push_str(", dialogue (centre) channel emphasised");
        }
    }
    args.extend(["-vn", "-f", "s16le", "-ac", "1", "-acodec", "pcm_s16le", "-ar", "16000", "-"].map(String::from));

    let mut child = Command::new(tool("ffmpeg"))
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start ffmpeg (install it with: brew install ffmpeg)")?;

    let mut raw = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut raw)?;
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).ok();
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg failed to read the file: {}", err.trim());
    }
    if raw.len() < 2 {
        bail!("the file has no audio track");
    }
    let samples = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect();
    Ok(Decoded { samples, note })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(index: u32, ch: u32, layout: &str, lang: &str) -> AudioStream {
        let mut tags = std::collections::HashMap::new();
        tags.insert("language".to_string(), lang.to_string());
        AudioStream { index, channels: ch, channel_layout: layout.into(), tags }
    }

    #[test]
    fn picks_spoken_language_track() {
        let s = [st(1, 2, "stereo", "eng"), st(2, 6, "5.1(side)", "jpn")];
        assert_eq!(pick_stream(&s, "ja").unwrap().index, 2);
        assert_eq!(pick_stream(&s, "en").unwrap().index, 1);
        assert_eq!(pick_stream(&s, "ko").unwrap().index, 1);
    }

    #[test]
    fn centre_channel_only_for_surround() {
        assert!(mono_filter(&st(1, 6, "5.1(side)", "jpn")).is_some());
        assert!(mono_filter(&st(1, 8, "7.1", "jpn")).is_some());
        assert!(mono_filter(&st(1, 2, "stereo", "jpn")).is_none());
        assert!(mono_filter(&st(1, 1, "mono", "jpn")).is_none());
    }
}
