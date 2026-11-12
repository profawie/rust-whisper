//! HTTP API (same endpoints as the original Python server) + embedded WASM UI.
use std::convert::Infallible;
use std::net::{SocketAddr, UdpSocket};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path as AxPath, Query, State as AxState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{Config, IpInfo, LanguageModelPref, MAX_BYTES};
use futures_util::{Stream, StreamExt};
use rust_embed::RustEmbed;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::models;
use crate::state::{human, lock, App};

#[derive(RustEmbed)]
#[folder = "../web/dist"]
struct Assets;

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(code: StatusCode, msg: impl Into<String>) -> ApiError {
    (code, Json(serde_json::json!({ "detail": msg.into() })))
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/ip", get(ip))
        .route("/api/config", get(config))
        .route("/api/state", get(state))
        .route("/api/preflight", get(preflight))
        .route("/api/language-model", post(set_language_model))
        .route("/api/transcribe", post(transcribe).layer(DefaultBodyLimit::disable()))
        .route("/api/download", get(download))
        .route("/api/clear", post(clear))
        .route("/api/events", get(events))
        .route("/{*path}", get(asset))
        .with_state(app)
}

pub fn startup(app: &App) {
    // drop leftovers from a previous crash
    let _ = std::fs::remove_dir_all(&app.paths.work);
    let _ = std::fs::create_dir_all(&app.paths.work);
    let _ = std::fs::create_dir_all(&app.paths.models);
    if let Some(free) = free_space(&app.paths.work) {
        if free < 5 * 1024 * 1024 * 1024 {
            let msg = format!("Low disk space on this Mac: only {} free. Uploads need the file size plus 1 GB.", human(free));
            eprintln!("  WARNING: {msg}");
            app.warn(msg);
        }
    }
}

// ------------------------------------------------------------- static
fn serve_asset(path: &str) -> Response {
    match Assets::get(path) {
        Some(f) => {
            let mime = f.metadata.mimetype().to_string();
            ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache".into())], f.data.into_owned()).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn index() -> Response {
    serve_asset("index.html")
}

async fn asset(AxPath(path): AxPath<String>) -> Response {
    serve_asset(&path)
}

// ------------------------------------------------------------- info
pub fn lan_ip() -> Option<String> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("10.255.255.255:1").ok()?; // UDP: no packet is sent
    Some(s.local_addr().ok()?.ip().to_string())
}

async fn ip(AxState(app): AxState<App>) -> Json<IpInfo> {
    Json(IpInfo { ip: if app.desktop { None } else { lan_ip() }, port: app.port })
}

async fn config(AxState(app): AxState<App>) -> Json<Config> {
    Json(Config {
        models: models::catalogue(),
        languages: models::languages(),
        default_model: models::DEFAULT_MODEL.into(),
        default_language: models::DEFAULT_LANGUAGE.into(),
        max_bytes: MAX_BYTES,
        language_models: lock(&app.prefs).clone(),
        desktop: app.desktop,
    })
}

async fn state(AxState(app): AxState<App>) -> Json<common::State> {
    Json(app.snapshot())
}

async fn set_language_model(AxState(app): AxState<App>, Json(p): Json<LanguageModelPref>) -> Result<Json<serde_json::Value>, ApiError> {
    let m = models::find(&p.model).ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("Unknown model '{}'", p.model)))?;
    if !models::languages().iter().any(|l| l.code == p.language) {
        return Err(err(StatusCode::BAD_REQUEST, format!("Unsupported language '{}'", p.language)));
    }
    if !m.supports(&p.language) {
        return Err(err(StatusCode::BAD_REQUEST, format!("{} can't be used for '{}'.", m.label, p.language)));
    }
    let mut prefs = lock(&app.prefs);
    prefs.insert(p.language, p.model);
    if let Err(e) = models::save_prefs(&app.paths.settings, &prefs) {
        app.warn(format!("Could not save model choice: {e}"));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ------------------------------------------------------------- upload + job
#[derive(Deserialize)]
struct TranscribeQuery {
    model: Option<String>,
    language: Option<String>,
}

fn free_space(path: &std::path::Path) -> Option<u64> {
    // `df -k` keeps us free of platform-specific statvfs bindings.
    let out = std::process::Command::new("df").arg("-k").arg(path).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    line.split_whitespace().nth(3)?.parse::<u64>().ok().map(|k| k * 1024)
}

/// Free space the Mac must keep beyond the upload itself: macOS needs room for swap
/// while a large model runs, and a full disk makes the upload fail half-way.
const DISK_RESERVE: u64 = 1024 * 1024 * 1024;

/// Why a file of `size` bytes can't be accepted right now, if it can't.
fn upload_blocker(app: &App, size: u64) -> Option<ApiError> {
    if size > MAX_BYTES {
        return Some(err(StatusCode::PAYLOAD_TOO_LARGE, format!("File is {}; the limit is {}.", human(size), human(MAX_BYTES))));
    }
    let status = app.snapshot().status;
    if status == "uploading" || status == "processing" {
        return Some(err(StatusCode::CONFLICT, "Another file is already being uploaded or processed."));
    }
    if size > 0 {
        if let Some(free) = free_space(&app.paths.work) {
            if size + DISK_RESERVE > free {
                return Some(err(
                    StatusCode::INSUFFICIENT_STORAGE,
                    format!(
                        "The server's disk is full: {} free, this file needs {} plus 1 GB to spare. Free up space on the Mac and try again.",
                        human(free),
                        human(size)
                    ),
                ));
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct PreflightQuery {
    size: Option<u64>,
}

/// Lets a client check before sending a large file (a refused upload otherwise
/// shows up on the client as a reset connection, not as the real reason).
async fn preflight(AxState(app): AxState<App>, Query(q): Query<PreflightQuery>) -> Result<Json<serde_json::Value>, ApiError> {
    match upload_blocker(&app, q.size.unwrap_or(0)) {
        Some(e) => Err(e),
        None => Ok(Json(serde_json::json!({ "ok": true, "free": free_space(&app.paths.work) }))),
    }
}

/// Raw-body upload (not multipart) so a 10 GB file is streamed straight to disk.
async fn transcribe(
    AxState(app): AxState<App>,
    Query(q): Query<TranscribeQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<serde_json::Value>, ApiError> {
    let model = q.model.unwrap_or_else(|| models::DEFAULT_MODEL.into());
    let language = q.language.unwrap_or_else(|| models::DEFAULT_LANGUAGE.into());
    let m = models::find(&model).ok_or_else(|| err(StatusCode::BAD_REQUEST, format!("Unknown model '{model}'")))?;
    if !models::languages().iter().any(|l| l.code == language) {
        return Err(err(StatusCode::BAD_REQUEST, format!("Unsupported language '{language}'")));
    }
    if !m.supports(&language) {
        return Err(err(StatusCode::BAD_REQUEST, format!("{} can't be used for '{language}'; pick another model.", m.label)));
    }

    let raw_name = headers.get("x-filename").and_then(|v| v.to_str().ok()).unwrap_or("upload");
    let filename = percent_encoding::percent_decode_str(raw_name).decode_utf8_lossy().trim().to_string();
    let filename = if filename.is_empty() { "upload".to_string() } else { filename };
    let suffix = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| (1..=8).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".into());
    let declared: u64 = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
    if declared > MAX_BYTES {
        return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("File is {}; the limit is {}.", human(declared), human(MAX_BYTES))));
    }

    {
        let mut s = app.state();
        if s.status == "uploading" || s.status == "processing" {
            return Err(err(StatusCode::CONFLICT, "Another file is already being uploaded or processed."));
        }
        *s = common::State {
            status: "uploading".into(),
            stage: "Uploading".into(),
            filename: filename.clone(),
            model: model.clone(),
            language: language.clone(),
            ..Default::default()
        };
    }
    app.awake.hold(); // no idle sleep while uploading / processing
    let _ = std::fs::create_dir_all(&app.paths.work);
    if let Ok(rd) = std::fs::read_dir(&app.paths.work) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path()); // previous result is replaced
        }
    }
    *lock(&app.output) = None;

    let path = app.paths.work.join(format!("{}.{suffix}", uuid::Uuid::new_v4().simple()));
    app.info("-".repeat(48));
    app.info(format!("Receiving '{filename}' ({})", if declared > 0 { human(declared) } else { "unknown size".into() }));

    let abort = |msg: &str| {
        crate::awake::KeepAwake::release_later(&app);
        let _ = std::fs::remove_file(&path);
        app.update(|s| *s = common::State { status: "idle".into(), ..Default::default() });
        app.warn(msg.to_string());
    };

    if declared > 0 {
        if let Some(free) = free_space(&app.paths.work) {
            if declared + DISK_RESERVE > free {
                abort(&format!("Not enough disk space ({} free, need {} + 1 GB).", human(free), human(declared)));
                return Err(err(
                    StatusCode::INSUFFICIENT_STORAGE,
                    format!("The server's disk is full: {} free, this file needs {} plus 1 GB to spare.", human(free), human(declared)),
                ));
            }
        }
    }

    let mut file = match tokio::fs::File::create(&path).await {
        Ok(f) => tokio::io::BufWriter::with_capacity(4 << 20, f),
        Err(e) => {
            abort("Upload failed.");
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
    };
    let mut total = 0u64;
    let mut next_pct = 10u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(_) => {
                abort("Upload cancelled by the browser.");
                return Err(err(StatusCode::from_u16(499).unwrap(), "Upload cancelled."));
            }
        };
        total += chunk.len() as u64;
        if total > MAX_BYTES {
            abort("Upload failed.");
            return Err(err(StatusCode::PAYLOAD_TOO_LARGE, format!("File exceeds the {} limit.", human(MAX_BYTES))));
        }
        if let Err(e) = file.write_all(&chunk).await {
            let msg = if e.kind() == std::io::ErrorKind::StorageFull {
                "Upload failed: the server's disk is full. Free up space on the Mac.".to_string()
            } else {
                format!("Upload failed: {e}")
            };
            abort(&msg);
            return Err(err(StatusCode::INSUFFICIENT_STORAGE, msg));
        }
        if declared > 0 {
            let pct = total * 100 / declared;
            if pct >= next_pct {
                app.info(format!("Upload: {pct}% ({})", human(total)));
                next_pct = (pct / 10 + 1) * 10;
            }
        }
    }
    if let Err(e) = file.flush().await {
        abort("Upload failed.");
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    drop(file);
    if total == 0 {
        abort("Empty upload.");
        return Err(err(StatusCode::BAD_REQUEST, "Empty file."));
    }

    app.ok(format!("Upload complete ({})", human(total)));
    app.update(|s| {
        s.status = "processing".into();
        s.stage = "Starting".into();
        s.progress = 0.0;
    });
    let app2 = app.clone();
    std::thread::spawn(move || crate::engine::run_job(app2, path, filename, model, language));
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn download(AxState(app): AxState<App>) -> Result<Response, ApiError> {
    let path = lock(&app.output).clone().filter(|p| p.exists()).ok_or_else(|| err(StatusCode::NOT_FOUND, "No subtitle available."))?;
    let name = app.snapshot().output_name;
    let name = if name.is_empty() { "subtitle.srt".to_string() } else { name };
    let data = tokio::fs::read(&path).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let encoded = percent_encoding::utf8_percent_encode(&name, percent_encoding::NON_ALPHANUMERIC).to_string();
    let ascii: String = name.chars().map(|c| if c.is_ascii() && c != '"' { c } else { '_' }).collect();
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-subrip".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")),
        ],
        data,
    )
        .into_response())
}

async fn clear(AxState(app): AxState<App>) -> Result<Json<serde_json::Value>, ApiError> {
    {
        let mut s = app.state();
        if s.status == "uploading" || s.status == "processing" {
            return Err(err(StatusCode::CONFLICT, "A job is running; wait for it to finish."));
        }
        *s = common::State { status: "idle".into(), ..Default::default() };
    }
    let mut freed = 0u64;
    if let Ok(rd) = std::fs::read_dir(&app.paths.work) {
        for e in rd.flatten() {
            freed += e.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    let _ = std::fs::remove_dir_all(&app.paths.work);
    let _ = std::fs::create_dir_all(&app.paths.work);
    *lock(&app.output) = None;
    *lock(&app.model_cache) = Default::default(); // frees the loaded model (RAM / GPU memory)
    app.reset_log();
    app.ok(format!("Cleared all files and memory ({} freed). Downloaded models were kept.", human(freed)));
    Ok(Json(serde_json::json!({ "ok": true, "freed": freed })))
}

/// Server-Sent Events: `log` lines + `state` snapshots. The full log is replayed on connect.
async fn events(AxState(app): AxState<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    struct S {
        app: App,
        epoch: u64,
        idx: usize,
        last: Option<common::State>,
        first: bool,
        queue: std::collections::VecDeque<Event>,
    }
    let init = S { epoch: app.epoch(), app, idx: 0, last: None, first: true, queue: Default::default() };
    let stream = futures_util::stream::unfold(init, |mut s| async move {
        loop {
            if let Some(ev) = s.queue.pop_front() {
                return Some((Ok(ev), s));
            }
            if s.first {
                s.first = false;
                s.queue.push_back(Event::default().event("reset").data("{}").retry(Duration::from_millis(2000)));
                continue;
            }
            if s.epoch != s.app.epoch() {
                s.epoch = s.app.epoch();
                s.idx = 0;
                s.queue.push_back(Event::default().event("reset").data("{}"));
            }
            for line in s.app.log_since(s.idx) {
                s.idx += 1;
                s.queue.push_back(Event::default().event("log").data(serde_json::to_string(&line).unwrap()));
            }
            let snap = s.app.snapshot();
            if s.last.as_ref() != Some(&snap) {
                s.queue.push_back(Event::default().event("state").data(serde_json::to_string(&snap).unwrap()));
                s.last = Some(snap);
            }
            if s.queue.is_empty() {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Bind and serve until the process exits.
pub async fn serve(app: App, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(app, listener).await
}

pub async fn serve_listener(app: App, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    startup(&app);
    axum::serve(listener, router(app)).await?;
    Ok(())
}
