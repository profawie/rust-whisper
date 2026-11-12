//! Talking to the rust-whisper server (same HTTP API the web UI uses).
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use common::{Config, LanguageModelPref, State};

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout_read(timeout).build()
}

/// How long the client keeps retrying while the server can't be reached
/// (Mac asleep, Wi-Fi reconnecting, server restarting) before giving up.
const RECONNECT_FOR: Duration = Duration::from_secs(20 * 60);

fn is_transport(e: &anyhow::Error) -> bool {
    e.to_string().starts_with(UNREACHABLE)
}

const UNREACHABLE: &str = "server not reachable";

fn detail(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("detail").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or(body);
            anyhow!("server said {code}: {msg}")
        }
        ureq::Error::Transport(t) => {
            // e.g. os error 10065 "unreachable host": the Mac went to sleep or Wi-Fi dropped
            anyhow!("{UNREACHABLE} ({t}). Is the Mac awake and on the same network?")
        }
    }
}

/// Run `f`, retrying while the server is unreachable (up to RECONNECT_FOR).
fn retrying<T>(tx: &Sender<JobEvent>, cancel: &Arc<AtomicBool>, pct: f32, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let started = std::time::Instant::now();
    let mut tries = 0;
    loop {
        match f() {
            Err(e) if is_transport(&e) && started.elapsed() < RECONNECT_FOR => {
                tries += 1;
                let _ = tx.send(JobEvent::Progress {
                    stage: format!("Server not reachable, retrying ({tries})…"),
                    pct,
                });
                for _ in 0..10 {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("cancelled");
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            other => return other,
        }
    }
}

pub fn base_url(ip: &str, port: &str) -> String {
    let ip = ip.trim().trim_start_matches("http://").trim_end_matches('/');
    format!("http://{ip}:{}", port.trim())
}

pub fn config(base: &str) -> Result<Config> {
    let resp = agent(Duration::from_secs(10)).get(&format!("{base}/api/config")).call().map_err(detail)?;
    resp.into_json().context("not a rust-whisper server")
}

pub fn state(base: &str) -> Result<State> {
    let resp = agent(Duration::from_secs(10)).get(&format!("{base}/api/state")).call().map_err(detail)?;
    Ok(resp.into_json()?)
}

/// Can the server take a file of `size` bytes now? Err carries the server's reason.
pub fn preflight(base: &str, size: u64) -> Result<()> {
    match agent(Duration::from_secs(15)).get(&format!("{base}/api/preflight?size={size}")).call() {
        Ok(_) => Ok(()),
        // older servers without this endpoint
        Err(ureq::Error::Status(404, _)) => Ok(()),
        Err(e) => Err(detail(e)),
    }
}

pub fn set_language_model(base: &str, language: &str, model: &str) -> Result<()> {
    agent(Duration::from_secs(10))
        .post(&format!("{base}/api/language-model"))
        .send_json(LanguageModelPref { language: language.into(), model: model.into() })
        .map_err(detail)?;
    Ok(())
}

/// Progress reported to the UI while a file is processed.
pub enum JobEvent {
    Started,
    Progress { stage: String, pct: f32 },
    Finished { video: PathBuf, srt: PathBuf },
    Failed { video: PathBuf, error: String },
}

struct CountingReader<R> {
    inner: R,
    sent: u64,
    total: u64,
    next: u64,
    tx: Sender<JobEvent>,
    cancel: Arc<AtomicBool>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.sent += n as u64;
        if self.sent >= self.next || n == 0 {
            self.next = self.sent + (self.total / 200).max(1 << 20);
            let pct = if self.total > 0 { self.sent as f32 * 100.0 / self.total as f32 } else { 0.0 };
            let _ = self.tx.send(JobEvent::Progress { stage: format!("Uploading {pct:.0}%"), pct });
        }
        Ok(n)
    }
}

/// Upload one video, wait for the server to finish, save `<video>.srt` next to it.
pub fn process(
    base: &str,
    video: &Path,
    model: &str,
    language: &str,
    tx: &Sender<JobEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<PathBuf> {
    // wait while the server is busy with someone else's file
    loop {
        let s = retrying(tx, cancel, 0.0, || state(base))?;
        if s.status != "uploading" && s.status != "processing" {
            break;
        }
        let _ = tx.send(JobEvent::Progress { stage: "Server busy, waiting…".into(), pct: 0.0 });
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled");
        }
        std::thread::sleep(Duration::from_secs(3));
    }

    let file = File::open(video).with_context(|| format!("opening {}", video.display()))?;
    let total = file.metadata()?.len();
    // ask first: a refused upload would otherwise look like a dropped connection
    retrying(tx, cancel, 0.0, || preflight(base, total))?;
    let name = video.file_name().and_then(|n| n.to_str()).unwrap_or("video");
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC).to_string();
    let reader = CountingReader { inner: file, sent: 0, total, next: 0, tx: tx.clone(), cancel: cancel.clone() };
    let url = format!(
        "{base}/api/transcribe?model={}&language={}",
        percent_encoding::utf8_percent_encode(model, percent_encoding::NON_ALPHANUMERIC),
        percent_encoding::utf8_percent_encode(language, percent_encoding::NON_ALPHANUMERIC)
    );
    let sent = agent(Duration::from_secs(3600))
        .post(&url)
        .set("X-Filename", &encoded)
        .set("Content-Length", &total.to_string())
        .send(reader)
        .map_err(detail);
    if let Err(e) = sent {
        // the server closes the connection when it refuses an upload half-way
        // (e.g. its disk filled up): ask it why instead of reporting "not reachable"
        if is_transport(&e) {
            std::thread::sleep(Duration::from_millis(500));
            if let Err(reason) = preflight(base, total) {
                if !is_transport(&reason) {
                    return Err(reason);
                }
            }
        }
        return Err(e);
    }

    // follow the job (a lost connection is retried: the server keeps working meanwhile)
    let mut last_pct = 0.0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("cancelled (the server finishes the current file on its own)");
        }
        std::thread::sleep(Duration::from_millis(1000));
        let s = retrying(tx, cancel, last_pct, || state(base))?;
        last_pct = s.progress as f32;
        match s.status.as_str() {
            "done" => break,
            "error" => bail!("{}", s.error),
            "idle" => bail!("the job was cleared on the server"),
            _ => {
                let _ = tx.send(JobEvent::Progress { stage: s.stage.clone(), pct: s.progress as f32 });
            }
        }
    }

    let _ = tx.send(JobEvent::Progress { stage: "Downloading subtitle".into(), pct: 100.0 });
    let srt = retrying(tx, cancel, 100.0, || {
        let resp = agent(Duration::from_secs(60)).get(&format!("{base}/api/download")).call().map_err(detail)?;
        let mut srt = Vec::new();
        resp.into_reader().read_to_end(&mut srt)?;
        Ok(srt)
    })?;
    let out = video.with_extension("srt");
    std::fs::write(&out, srt).with_context(|| format!("writing {}", out.display()))?;
    Ok(out)
}
