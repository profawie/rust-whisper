//! Shared engine + HTTP API used by both the server and the desktop app.
pub mod api;
pub mod audio;
pub mod cleanup;
pub mod engine;
pub mod models;
pub mod state;

pub use state::{App, Paths, Shared};

/// Silence whisper.cpp / ggml console output (progress goes to the UI log instead).
pub fn quiet_whisper_logs() {
    whisper_rs::install_logging_hooks();
}
