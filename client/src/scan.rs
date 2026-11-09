//! Folder scan: build two trees, videos without and videos with a subtitle.
use std::path::{Path, PathBuf};

pub const VIDEO_EXT: &[&str] = &[
    "mp4", "mkv", "mov", "avi", "webm", "ts", "m2ts", "mts", "m4v", "wmv", "flv", "mpg", "mpeg", "3gp", "vob", "ogv",
];

#[derive(Clone, Debug)]
pub struct Video {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// Subtitles found next to the video: `name.srt`, `name.en.srt`, ...
    pub srts: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default)]
pub struct Folder {
    pub path: PathBuf,
    pub name: String,
    pub folders: Vec<Folder>,
    pub videos: Vec<Video>,
}

impl Folder {
    pub fn count(&self) -> usize {
        self.videos.len() + self.folders.iter().map(Folder::count).sum::<usize>()
    }
    pub fn all_videos<'a>(&'a self, out: &mut Vec<&'a Video>) {
        out.extend(self.videos.iter());
        for f in &self.folders {
            f.all_videos(out);
        }
    }
}

pub struct Scan {
    pub without: Folder,
    pub with: Folder,
}

fn sort_key(s: &str) -> String {
    s.to_lowercase()
}

fn is_hidden(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('$') || name.eq_ignore_ascii_case("System Volume Information")
}

pub fn scan(root: &Path) -> Scan {
    let (without, with) = scan_dir(root, 0);
    let name = root.display().to_string();
    Scan {
        without: without.unwrap_or_else(|| Folder { path: root.into(), name: name.clone(), ..Default::default() }),
        with: with.unwrap_or_else(|| Folder { path: root.into(), name, ..Default::default() }),
    }
}

/// Returns (folder of videos without srt, folder of videos with srt); `None` when empty.
fn scan_dir(dir: &Path, depth: usize) -> (Option<Folder>, Option<Folder>) {
    let name = if depth == 0 {
        dir.display().to_string()
    } else {
        dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    };
    let mut without = Folder { path: dir.into(), name: name.clone(), ..Default::default() };
    let mut with = Folder { path: dir.into(), name, ..Default::default() };
    let Ok(rd) = std::fs::read_dir(dir) else { return (None, None) };

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().into_owned();
        if is_hidden(&fname) {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            dirs.push(e.path());
        } else {
            files.push((fname, e.path(), e.metadata().map(|m| m.len()).unwrap_or(0)));
        }
    }

    let srts: Vec<&(String, PathBuf, u64)> =
        files.iter().filter(|(n, _, _)| n.to_lowercase().ends_with(".srt")).collect();
    for (fname, path, size) in &files {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if !VIDEO_EXT.contains(&ext.as_str()) {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        let mut found: Vec<PathBuf> = srts
            .iter()
            .filter(|(n, _, _)| {
                let n = n.to_lowercase();
                let base = &n[..n.len() - 4];
                base == stem || base.strip_prefix(&stem).is_some_and(|rest| rest.starts_with('.'))
            })
            .map(|(_, p, _)| p.clone())
            .collect();
        found.sort_by_key(|p| (p.as_os_str().len(), p.clone())); // "name.srt" before "name.en.srt"
        let v = Video { path: path.clone(), name: fname.clone(), size: *size, srts: found };
        if v.srts.is_empty() {
            without.videos.push(v);
        } else {
            with.videos.push(v);
        }
    }
    if depth < 32 {
        for d in dirs {
            let (a, b) = scan_dir(&d, depth + 1);
            without.folders.extend(a);
            with.folders.extend(b);
        }
    }
    for f in [&mut without, &mut with] {
        f.videos.sort_by_key(|v| sort_key(&v.name));
        f.folders.sort_by_key(|x| sort_key(&x.name));
    }
    let keep = |f: Folder| if f.videos.is_empty() && f.folders.is_empty() { None } else { Some(f) };
    (keep(without), keep(with))
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
