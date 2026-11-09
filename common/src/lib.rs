//! Types shared by the server (core) and the WASM web UI.
use serde::{Deserialize, Serialize};

pub const MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub label: String,
    pub size: String,
    pub note: String,
    /// ggml model used for English audio.
    pub en: String,
    /// Multilingual ggml model used to translate other languages to English
    /// (`None` = this model cannot translate, e.g. large-v3-turbo).
    pub multi: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Language {
    pub code: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub models: Vec<ModelInfo>,
    pub languages: Vec<Language>,
    pub default_model: String,
    pub default_language: String,
    pub max_bytes: u64,
    /// Model chosen for each spoken language (remembered on the server).
    pub language_models: std::collections::BTreeMap<String, String>,
    /// True when running inside the desktop app (no LAN address to show).
    pub desktop: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    /// idle | uploading | processing | done | error
    pub status: String,
    pub stage: String,
    pub progress: f64,
    pub filename: String,
    pub model: String,
    pub language: String,
    pub error: String,
    pub output_name: String,
    pub output_size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LogLine {
    pub t: String,
    /// info | ok | warn | error
    pub level: String,
    pub msg: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IpInfo {
    pub ip: Option<String>,
    pub port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LanguageModelPref {
    pub language: String,
    pub model: String,
}

/// Languages whose translation to English gets the extra clean-up pass
/// (VAD chunking, retry of untranslated lines, repetition removal).
pub fn is_cjk_language(code: &str) -> bool {
    matches!(code, "ja" | "zh" | "ko")
}
