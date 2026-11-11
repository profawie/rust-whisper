//! Shared job state and log, read by the HTTP API / SSE stream.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use common::{LogLine, State};

use crate::engine::ModelCache;

pub struct Paths {
    pub work: PathBuf,
    pub models: PathBuf,
    pub settings: PathBuf,
}

impl Paths {
    /// `$WHISPER_HOME`, or `~/.cache/rust-whisper`. Models are shared by the
    /// server and the desktop app, so they are downloaded only once.
    pub fn from_env() -> Self {
        let base = std::env::var_os("WHISPER_HOME").map(PathBuf::from).unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
            home.join(".cache").join("rust-whisper")
        });
        Paths { work: base.join("work"), models: base.join("models"), settings: base.join("settings.json") }
    }
}

pub struct Shared {
    pub paths: Paths,
    pub port: u16,
    pub desktop: bool,
    state: Mutex<State>,
    log: Mutex<Vec<LogLine>>,
    epoch: AtomicU64,
    pub output: Mutex<Option<PathBuf>>,
    pub prefs: Mutex<BTreeMap<String, String>>,
    pub model_cache: Mutex<ModelCache>,
    pub awake: crate::awake::KeepAwake,
}

pub type App = Arc<Shared>;

impl Shared {
    pub fn new(paths: Paths, port: u16, desktop: bool) -> App {
        let prefs = crate::models::load_prefs(&paths.settings);
        Arc::new(Shared {
            paths,
            port,
            desktop,
            state: Mutex::new(State { status: "idle".into(), ..Default::default() }),
            log: Mutex::new(Vec::new()),
            epoch: AtomicU64::new(0),
            output: Mutex::new(None),
            prefs: Mutex::new(prefs),
            model_cache: Mutex::new(ModelCache::default()),
            awake: Default::default(),
        })
    }

    pub fn log(&self, level: &str, msg: impl Into<String>) {
        let line = LogLine {
            t: chrono::Local::now().format("%H:%M:%S").to_string(),
            level: level.into(),
            msg: msg.into(),
        };
        lock(&self.log).push(line);
    }
    pub fn info(&self, msg: impl Into<String>) {
        self.log("info", msg)
    }
    pub fn ok(&self, msg: impl Into<String>) {
        self.log("ok", msg)
    }
    pub fn warn(&self, msg: impl Into<String>) {
        self.log("warn", msg)
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }
    pub fn snapshot(&self) -> State {
        self.state().clone()
    }
    pub fn update(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.state())
    }
    pub fn set_progress(&self, stage: &str, progress: f64) {
        self.update(|s| {
            s.stage = stage.into();
            s.progress = progress;
        })
    }

    /// Log lines from `from`, plus the current epoch (bumped when the log is wiped).
    pub fn log_since(&self, from: usize) -> Vec<LogLine> {
        let log = lock(&self.log);
        log.get(from..).map(|s| s.to_vec()).unwrap_or_default()
    }
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub fn reset_log(&self) {
        lock(&self.log).clear();
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn human(n: u64) -> String {
    let mut v = n as f64;
    for unit in ["B", "KB", "MB", "GB"] {
        if v < 1024.0 || unit == "GB" {
            return if unit == "B" { format!("{v:.0} {unit}") } else { format!("{v:.1} {unit}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}
