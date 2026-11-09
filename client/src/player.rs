//! Open videos in VLC and folders in Explorer.
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};

/// Locate vlc.exe: Program Files, the registry (custom install dir), then PATH.
pub fn find_vlc() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
            if let Some(p) = std::env::var_os(var) {
                candidates.push(PathBuf::from(p).join("VideoLAN").join("VLC").join("vlc.exe"));
            }
        }
        candidates.push(r"C:\Program Files\VideoLAN\VLC\vlc.exe".into());
        candidates.push(r"C:\Program Files (x86)\VideoLAN\VLC\vlc.exe".into());
        for key in [r"HKLM\SOFTWARE\VideoLAN\VLC", r"HKLM\SOFTWARE\WOW6432Node\VideoLAN\VLC"] {
            if let Some(dir) = reg_value(key, "InstallDir") {
                candidates.push(PathBuf::from(dir).join("vlc.exe"));
            }
        }
        if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
            return Some(found);
        }
        return which("vlc.exe");
    }
    #[cfg(target_os = "macos")]
    {
        let app = PathBuf::from("/Applications/VLC.app/Contents/MacOS/VLC");
        if app.is_file() {
            return Some(app);
        }
    }
    #[allow(unreachable_code)]
    which("vlc")
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

#[cfg(windows)]
fn reg_value(key: &str, value: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = Command::new("reg").args(["query", key, "/v", value]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.trim_start().starts_with(value))?;
    let idx = line.find("REG_SZ")?;
    Some(line[idx + 6..].trim().to_string())
}

/// Play `video` in VLC, with `srt` loaded as the subtitle track when given.
pub fn play(vlc: Option<&Path>, video: &Path, srt: Option<&Path>) -> Result<()> {
    let Some(vlc) = vlc else {
        bail!("VLC was not found. Install it from https://www.videolan.org/ (or set its path in the header).");
    };
    let mut cmd = Command::new(vlc);
    if let Some(s) = srt {
        cmd.arg(format!("--sub-file={}", s.display()));
    }
    cmd.arg(video);
    cmd.spawn()?;
    Ok(())
}

/// Show the file selected in Explorer (Finder on macOS).
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    {
        let _ = Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg("-R").arg(path).spawn();
    }
}
