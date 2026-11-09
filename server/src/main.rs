//! whisper-server: upload one video/audio file, get an English .srt back.
use std::net::SocketAddr;

use whisper_core::{api, Paths, Shared};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    whisper_core::quiet_whisper_logs();
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8888);
    let app = Shared::new(Paths::from_env(), port, false);

    println!("\n  Local:   http://127.0.0.1:{port}");
    if let Some(ip) = api::lan_ip() {
        println!("  Network: http://{ip}:{port}\n");
    }
    println!("  Models:  {}", app.paths.models.display());
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    api::serve(app, addr).await
}
