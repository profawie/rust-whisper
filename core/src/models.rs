//! Model catalogue, per-language model preferences and model downloads.
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use common::{Language, ModelInfo};

pub const DEFAULT_MODEL: &str = "small";
pub const DEFAULT_LANGUAGE: &str = "en";
/// Model suggested the first time Japanese / Chinese / Korean is picked.
/// large-v2 translates CJK speech to English noticeably better than large-v3,
/// which tends to leave words untranslated.
pub const DEFAULT_CJK_MODEL: &str = "large-v2";

const HF_WHISPER: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";
const HF_VAD: &str = "https://huggingface.co/ggml-org/whisper-vad/resolve/main";
pub const VAD_MODEL: &str = "ggml-silero-v5.1.2.bin";

fn m(id: &str, label: &str, size: &str, note: &str, en: &str, multi: Option<&str>) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        label: label.into(),
        size: size.into(),
        note: note.into(),
        en: en.into(),
        multi: multi.map(Into::into),
    }
}

pub fn catalogue() -> Vec<ModelInfo> {
    vec![
        m("tiny", "Tiny", "~75 MB", "fastest, lowest accuracy", "tiny.en", Some("tiny")),
        m("base", "Base", "~145 MB", "fast", "base.en", Some("base")),
        m("small", "Small", "~480 MB", "good balance", "small.en", Some("small")),
        m("medium", "Medium", "~1.5 GB", "accurate", "medium.en", Some("medium")),
        m("large-v2", "Large v2", "~3 GB", "best for Japanese/Chinese → English", "large-v2", Some("large-v2")),
        m("large-v2-q5", "Large v2 (q5)", "~1.1 GB", "near Large v2 quality, less RAM", "large-v2-q5_0", Some("large-v2-q5_0")),
        m("large-v3", "Large v3", "~3 GB", "most accurate transcription", "large-v3", Some("large-v3")),
        m("large-v3-turbo", "Large v3 Turbo", "~1.6 GB", "English audio only", "large-v3-turbo", None),
    ]
}

pub fn languages() -> Vec<Language> {
    [
        ("en", "English"), ("auto", "Auto-detect"), ("ms", "Malay"), ("id", "Indonesian"), ("zh", "Chinese"),
        ("ja", "Japanese"), ("ko", "Korean"), ("ar", "Arabic"), ("hi", "Hindi"), ("ta", "Tamil"), ("th", "Thai"),
        ("vi", "Vietnamese"), ("tl", "Tagalog"), ("fr", "French"), ("de", "German"), ("es", "Spanish"),
        ("pt", "Portuguese"), ("it", "Italian"), ("nl", "Dutch"), ("ru", "Russian"), ("tr", "Turkish"),
        ("pl", "Polish"), ("uk", "Ukrainian"), ("sv", "Swedish"), ("el", "Greek"), ("he", "Hebrew"),
        ("fa", "Persian"), ("ur", "Urdu"), ("bn", "Bengali"),
    ]
    .into_iter()
    .map(|(c, n)| Language { code: c.into(), name: n.into() })
    .collect()
}

pub fn find(id: &str) -> Option<ModelInfo> {
    catalogue().into_iter().find(|m| m.id == id)
}

/// Model to pre-select for `language` when the user has not picked one yet.
pub fn default_for(language: &str) -> &'static str {
    if common::is_cjk_language(language) {
        DEFAULT_CJK_MODEL
    } else {
        DEFAULT_MODEL
    }
}

// ------------------------------------------------------------- preferences
/// The model the user picked for each spoken language, saved in settings.json.
pub fn load_prefs(path: &Path) -> BTreeMap<String, String> {
    let mut prefs: BTreeMap<String, String> = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| serde_json::from_value(v.get("language_models")?.clone()).ok())
        .unwrap_or_default();
    prefs.retain(|_, m| find(m).is_some());
    for l in languages() {
        prefs.entry(l.code.clone()).or_insert_with(|| default_for(&l.code).to_string());
    }
    prefs
}

pub fn save_prefs(path: &Path, prefs: &BTreeMap<String, String>) -> Result<()> {
    let body = serde_json::to_vec_pretty(&serde_json::json!({ "language_models": prefs }))?;
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

// ------------------------------------------------------------- downloads
pub fn ggml_path(models_dir: &Path, arch: &str) -> PathBuf {
    models_dir.join(format!("ggml-{arch}.bin"))
}

/// Make sure `ggml-<arch>.bin` exists, downloading it on first use.
pub fn ensure_model(models_dir: &Path, arch: &str, on_progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    let path = ggml_path(models_dir, arch);
    if !path.exists() {
        download(&format!("{HF_WHISPER}/ggml-{arch}.bin"), &path, on_progress)?;
    }
    Ok(path)
}

pub fn ensure_vad(models_dir: &Path, on_progress: impl FnMut(u64, u64)) -> Result<PathBuf> {
    let path = models_dir.join(VAD_MODEL);
    if !path.exists() {
        download(&format!("{HF_VAD}/{VAD_MODEL}"), &path, on_progress)?;
    }
    Ok(path)
}

fn download(url: &str, dest: &Path, mut on_progress: impl FnMut(u64, u64)) -> Result<()> {
    fs::create_dir_all(dest.parent().unwrap())?;
    let resp = ureq::get(url).call().with_context(|| format!("downloading {url}"))?;
    let total: u64 = resp.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let tmp = dest.with_extension("part");
    let mut out = fs::File::create(&tmp)?;
    let mut reader = resp.into_reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        on_progress(done, total);
    }
    out.flush()?;
    drop(out);
    if total > 0 && done != total {
        let _ = fs::remove_file(&tmp);
        bail!("download of {url} was cut short ({done} of {total} bytes)");
    }
    fs::rename(&tmp, dest)?;
    Ok(())
}
