//! Keep the Mac awake while a file is uploaded or processed.
//!
//! On battery macOS puts an idle Mac to sleep after a few minutes even while the
//! server is busy; clients then see "unreachable host" (Windows os error 10065).
//! `caffeinate -i` blocks idle sleep for as long as it runs. It is started when a
//! job starts and stopped a few minutes after the job ends, so the client can
//! still download the subtitle. (Closing the lid still sleeps the Mac.)
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::state::{lock, App};

#[derive(Default)]
pub struct KeepAwake {
    child: Mutex<Option<Child>>,
    generation: Mutex<u64>,
}

/// Grace period after a job before the Mac may sleep again.
const LINGER: Duration = Duration::from_secs(180);

impl KeepAwake {
    pub fn hold(&self) {
        *lock(&self.generation) += 1;
        let mut child = lock(&self.child);
        if child.as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None))) {
            return; // already holding
        }
        if cfg!(target_os = "macos") {
            *child = Command::new("/usr/bin/caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()]) // ends with the server
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
    }

    /// Let the Mac sleep again after LINGER, unless another job starts first.
    pub fn release_later(app: &App) {
        let gen = *lock(&app.awake.generation);
        let app = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(LINGER);
            let busy = matches!(app.snapshot().status.as_str(), "uploading" | "processing");
            if !busy && *lock(&app.awake.generation) == gen {
                if let Some(mut c) = lock(&app.awake.child).take() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        });
    }
}
