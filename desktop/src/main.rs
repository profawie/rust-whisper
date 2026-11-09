//! Whisper desktop app: the same engine and the same Rust/WASM UI, in a native
//! window. The engine runs inside this process on a private localhost port, so
//! nothing is exposed to the network and no browser is needed.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use whisper_core::{api, Paths, Shared};
use wry::WebViewBuilder;

fn main() -> anyhow::Result<()> {
    whisper_core::quiet_whisper_logs();

    // ---- start the engine on 127.0.0.1:<random port>
    let rt = tokio::runtime::Runtime::new()?;
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let port = listener.local_addr()?.port();
    let app = Shared::new(Paths::from_env(), port, true);
    let server_app = app.clone();
    rt.spawn(async move {
        if let Err(e) = api::serve_listener(server_app, listener).await {
            eprintln!("server error: {e:#}");
        }
    });

    // ---- native window
    let event_loop = EventLoopBuilder::new().build();
    let window = WindowBuilder::new()
        .with_title("Whisper")
        .with_inner_size(LogicalSize::new(880.0, 980.0))
        .with_min_inner_size(LogicalSize::new(420.0, 600.0))
        .build(&event_loop)?;

    // Downloads go to ~/Downloads (no overwrite); Finder then shows the file.
    let last_download = Arc::new(Mutex::new(None::<PathBuf>));
    let (dl_start, dl_done) = (last_download.clone(), last_download.clone());
    let log_app = app.clone();
    let _webview = WebViewBuilder::new()
        .with_url(format!("http://127.0.0.1:{port}/"))
        .with_devtools(cfg!(debug_assertions))
        .with_download_started_handler(move |_url, path| {
            *dl_start.lock().unwrap() = Some(path.clone());
            true
        })
        .with_download_completed_handler(move |_url, path, ok| {
            let path = path.or_else(|| dl_done.lock().unwrap().take());
            match (ok, path) {
                (true, Some(p)) => {
                    log_app.ok(format!("Saved {}", p.display()));
                    let _ = std::process::Command::new("open").arg("-R").arg(&p).spawn();
                }
                (true, None) => log_app.ok("Saved to Downloads."),
                (false, _) => log_app.warn("Download failed."),
            }
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep_runtime = &rt;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}
