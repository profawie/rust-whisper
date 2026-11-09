//! Decode any video/audio file to 16 kHz mono f32 PCM with ffmpeg.
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

pub const SAMPLE_RATE: usize = 16_000;

/// ffmpeg from PATH, or from Homebrew when PATH is minimal (apps opened from Finder).
fn ffmpeg() -> std::path::PathBuf {
    let in_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join("ffmpeg")).find(|f| f.is_file()))
        .flatten();
    in_path
        .or_else(|| ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"].iter().map(Into::into).find(|p: &std::path::PathBuf| p.is_file()))
        .unwrap_or_else(|| "ffmpeg".into())
}

pub fn load_audio(path: &Path) -> Result<Vec<f32>> {
    let mut child = Command::new(ffmpeg())
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-threads", "0", "-i"])
        .arg(path)
        .args(["-vn", "-f", "s16le", "-ac", "1", "-acodec", "pcm_s16le", "-ar", "16000", "-"])
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
    Ok(raw
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect())
}
