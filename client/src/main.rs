//! Whisper Client (Windows): connect to a rust-whisper server, browse a folder,
//! send videos without subtitles to the server, play videos (with their .srt) in VLC.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod net;
mod player;
mod scan;

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use common::Config;
use eframe::egui::{self, Color32, RichText};
use net::JobEvent;
use scan::{Folder, Scan, Video};
use serde::{Deserialize, Serialize};

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Whisper Client")
            .with_inner_size([1150.0, 760.0])
            .with_min_inner_size([720.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("Whisper Client", options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}

// ------------------------------------------------------------------ settings
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    ip: String,
    port: String,
    folder: Option<PathBuf>,
    language: String,
    vlc: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { ip: "192.168.1.4".into(), port: "8888".into(), folder: None, language: "en".into(), vlc: None }
    }
}

fn settings_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| ".".into()).join("rust-whisper-client").join("settings.json")
}

impl Settings {
    fn load() -> Self {
        std::fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let p = settings_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(p, serde_json::to_vec_pretty(self).unwrap());
    }
}

// ------------------------------------------------------------------ app
enum Conn {
    Disconnected,
    Connecting(Receiver<anyhow::Result<Config>>),
    Connected(Config),
    Failed(String),
}

struct Job {
    video: PathBuf,
    model: String,
    language: String,
}

enum Action {
    Play(PathBuf, Option<PathBuf>),
    Reveal(PathBuf),
    Generate(Vec<PathBuf>),
    Toggle(PathBuf),
    SelectAll(Vec<PathBuf>, bool),
}

struct App {
    settings: Settings,
    conn: Conn,
    base: String,
    model: String,
    scan: Option<Scan>,
    scanning: Option<Receiver<Scan>>,
    selected: HashSet<PathBuf>,
    queue: VecDeque<Job>,
    current: Option<(PathBuf, String, f32)>,
    job_tx: Sender<(String, Job)>,
    events: Receiver<JobEvent>,
    cancel: Arc<AtomicBool>,
    vlc: Option<PathBuf>,
    message: Option<(String, bool)>,
    done_count: usize,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let settings = Settings::load();
        let (job_tx, job_rx) = channel::<(String, Job)>();
        let (ev_tx, events) = channel::<JobEvent>();
        let cancel = Arc::new(AtomicBool::new(false));
        spawn_worker(job_rx, ev_tx, cancel.clone(), ctx.clone());
        let vlc = settings.vlc.clone().filter(|p| p.is_file()).or_else(player::find_vlc);
        let mut app = App {
            settings,
            conn: Conn::Disconnected,
            base: String::new(),
            model: String::new(),
            scan: None,
            scanning: None,
            selected: HashSet::new(),
            queue: VecDeque::new(),
            current: None,
            job_tx,
            events,
            cancel,
            vlc,
            message: None,
            done_count: 0,
        };
        app.rescan(ctx);
        if settings_path().exists() {
            app.connect(ctx); // reconnect to the last server automatically
        }
        app
    }

    fn config(&self) -> Option<&Config> {
        match &self.conn {
            Conn::Connected(c) => Some(c),
            _ => None,
        }
    }

    fn connect(&mut self, ctx: &egui::Context) {
        self.settings.save();
        self.base = net::base_url(&self.settings.ip, &self.settings.port);
        let (tx, rx) = channel();
        let base = self.base.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(net::config(&base));
            ctx.request_repaint();
        });
        self.conn = Conn::Connecting(rx);
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        let Some(root) = self.settings.folder.clone() else { return };
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(scan::scan(&root));
            ctx.request_repaint();
        });
        self.scanning = Some(rx);
    }

    fn model_for(&self, language: &str) -> String {
        let Some(c) = self.config() else { return String::new() };
        let pick = c.language_models.get(language).cloned().unwrap_or_else(|| c.default_model.clone());
        let ok = c.models.iter().any(|m| m.id == pick && (language == "en" || m.multi.is_some()));
        if ok { pick } else { c.default_model.clone() }
    }

    fn enqueue(&mut self, videos: Vec<PathBuf>) {
        if self.config().is_none() {
            self.message = Some(("Connect to the server first.".into(), true));
            return;
        }
        let mut added = 0;
        for v in videos {
            let queued = self.queue.iter().any(|j| j.video == v) || self.current.as_ref().is_some_and(|c| c.0 == v);
            if !queued {
                self.queue.push_back(Job { video: v, model: self.model.clone(), language: self.settings.language.clone() });
                added += 1;
            }
        }
        self.message = Some((format!("Added {added} file(s) to the queue."), false));
        self.pump();
    }

    /// Hand the next queued file to the worker when it is idle.
    fn pump(&mut self) {
        if self.current.is_some() {
            return;
        }
        if let Some(job) = self.queue.pop_front() {
            self.cancel.store(false, Ordering::Relaxed);
            self.selected.remove(&job.video);
            self.current = Some((job.video.clone(), "Starting".into(), 0.0));
            let _ = self.job_tx.send((self.base.clone(), job));
        }
    }

    fn handle_events(&mut self, ctx: &egui::Context) {
        if let Conn::Connecting(rx) = &self.conn {
            if let Ok(res) = rx.try_recv() {
                self.conn = match res {
                    Ok(c) => Conn::Connected(c),
                    Err(e) => Conn::Failed(format!("{e:#}")),
                };
                if self.config().is_some() {
                    let lang = self.settings.language.clone();
                    let known = self.config().unwrap().languages.iter().any(|l| l.code == lang);
                    if !known {
                        self.settings.language = "en".into();
                    }
                    self.model = self.model_for(&self.settings.language.clone());
                }
            }
        }
        if let Some(rx) = &self.scanning {
            if let Ok(s) = rx.try_recv() {
                self.scan = Some(s);
                self.scanning = None;
            }
        }
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                JobEvent::Started => {}
                JobEvent::Progress { stage, pct } => {
                    if let Some(c) = &mut self.current {
                        c.1 = stage;
                        c.2 = pct;
                    }
                }
                JobEvent::Finished { video, srt } => {
                    self.done_count += 1;
                    self.message = Some((
                        format!("Saved {}", srt.file_name().unwrap_or_default().to_string_lossy()),
                        false,
                    ));
                    let _ = video;
                    self.current = None;
                    self.rescan(ctx);
                    self.pump();
                }
                JobEvent::Failed { video, error } => {
                    self.message = Some((
                        format!("{}: {error}", video.file_name().unwrap_or_default().to_string_lossy()),
                        true,
                    ));
                    self.current = None;
                    if !self.cancel.load(Ordering::Relaxed) {
                        self.pump();
                    }
                }
            }
        }
    }

    fn status_of(&self, p: &Path) -> Option<&'static str> {
        if self.current.as_ref().is_some_and(|c| c.0 == p) {
            Some("processing")
        } else if self.queue.iter().any(|j| j.video == p) {
            Some("queued")
        } else {
            None
        }
    }

    // ---------------------------------------------------------------- header
    fn header(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Whisper Client").strong().size(18.0));
            ui.separator();
            ui.label("Server IP");
            ui.add(egui::TextEdit::singleline(&mut self.settings.ip).desired_width(130.0));
            ui.label("Port");
            ui.add(egui::TextEdit::singleline(&mut self.settings.port).desired_width(55.0));
            let connecting = matches!(self.conn, Conn::Connecting(_));
            let label = if matches!(self.conn, Conn::Connected(_)) { "Reconnect" } else { "Connect" };
            if ui.add_enabled(!connecting, egui::Button::new(label)).clicked() {
                self.connect(&ctx);
            }
            match &self.conn {
                Conn::Disconnected => ui.label(RichText::new("○ not connected").color(Color32::GRAY)),
                Conn::Connecting(_) => ui.label("connecting…"),
                Conn::Connected(_) => ui.label(RichText::new(format!("● connected to {}", self.base)).color(Color32::from_rgb(16, 185, 129))),
                Conn::Failed(e) => ui.label(RichText::new(format!("✖ {e}")).color(Color32::from_rgb(239, 68, 68))),
            };
        });

        if let Some(cfg) = self.config().cloned() {
            ui.horizontal(|ui| {
                ui.label("Spoken language");
                let lang_name = |code: &str| {
                    cfg.languages.iter().find(|l| l.code == code).map(|l| {
                        if l.code == "en" || l.code == "auto" { l.name.clone() } else { format!("{} → English", l.name) }
                    })
                };
                let mut lang = self.settings.language.clone();
                egui::ComboBox::from_id_salt("language")
                    .width(190.0)
                    .selected_text(lang_name(&lang).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for l in &cfg.languages {
                            ui.selectable_value(&mut lang, l.code.clone(), lang_name(&l.code).unwrap_or_default());
                        }
                    });
                if lang != self.settings.language {
                    self.settings.language = lang.clone();
                    self.model = self.model_for(&lang);
                    self.settings.save();
                }

                ui.add_space(12.0);
                ui.label("Model");
                let english = self.settings.language == "en";
                let mut model = self.model.clone();
                let text = |m: &common::ModelInfo| format!("{} — {}, {}", m.label, m.size, m.note);
                let current = cfg.models.iter().find(|m| m.id == model).map(text).unwrap_or_default();
                egui::ComboBox::from_id_salt("model").width(330.0).selected_text(current).show_ui(ui, |ui| {
                    for m in &cfg.models {
                        let enabled = english || m.multi.is_some();
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.selectable_value(&mut model, m.id.clone(), text(m));
                        });
                    }
                });
                if model != self.model {
                    self.model = model.clone();
                    let (base, lang) = (self.base.clone(), self.settings.language.clone());
                    if let Conn::Connected(c) = &mut self.conn {
                        c.language_models.insert(lang.clone(), model.clone());
                    }
                    std::thread::spawn(move || {
                        let _ = net::set_language_model(&base, &lang, &model);
                    });
                }
            });
            let m = cfg.models.iter().find(|m| m.id == self.model);
            if let Some(m) = m {
                let hint = if self.settings.language == "en" {
                    format!("Will use \"{}\". Output: English SRT saved next to each video.", m.en)
                } else {
                    let mut h = format!(
                        "Will use multilingual \"{}\" and translate to English. The model is remembered per language.",
                        m.multi.clone().unwrap_or_default()
                    );
                    if common::is_cjk_language(&self.settings.language) && !m.id.starts_with("large-v2") {
                        h.push_str(" Tip: Large v2 gives the best Japanese/Chinese → English translations.");
                    }
                    h
                };
                ui.label(RichText::new(hint).small().color(Color32::GRAY));
            }
        }

        ui.horizontal(|ui| {
            ui.label("Folder");
            if ui.button("Browse…").clicked() {
                let mut dlg = rfd::FileDialog::new();
                if let Some(f) = &self.settings.folder {
                    dlg = dlg.set_directory(f);
                }
                if let Some(dir) = dlg.pick_folder() {
                    self.settings.folder = Some(dir);
                    self.settings.save();
                    self.selected.clear();
                    self.rescan(&ctx);
                }
            }
            if ui.add_enabled(self.settings.folder.is_some(), egui::Button::new("Refresh")).clicked() {
                self.rescan(&ctx);
            }
            match &self.vlc {
                Some(p) => ui.label(RichText::new("VLC ✔").color(Color32::from_rgb(16, 185, 129))).on_hover_text(p.display().to_string()),
                None => ui.label(RichText::new("VLC not found").color(Color32::from_rgb(245, 158, 11))),
            };
            if ui.small_button("Set VLC…").on_hover_text("Choose vlc.exe manually").clicked() {
                if let Some(p) = rfd::FileDialog::new().add_filter("VLC", &["exe"]).pick_file() {
                    self.vlc = Some(p.clone());
                    self.settings.vlc = Some(p);
                    self.settings.save();
                }
            }
            if self.scanning.is_some() {
                ui.spinner();
            }
            ui.separator();
            let folder = self.settings.folder.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(no folder chosen)".into());
            ui.add(egui::Label::new(RichText::new(&folder).monospace()).truncate()).on_hover_text(folder);
        });
        ui.add_space(4.0);
    }

    // ---------------------------------------------------------------- footer
    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        if let Some((video, stage, pct)) = &self.current {
            let name = video.file_name().unwrap_or_default().to_string_lossy().into_owned();
            ui.horizontal(|ui| {
                ui.label(RichText::new(&name).strong());
                ui.label(RichText::new(stage).color(Color32::GRAY));
                if !self.queue.is_empty() {
                    ui.label(format!("· {} more in queue", self.queue.len()));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Cancel").on_hover_text("Stop after the current step and clear the queue").clicked() {
                        self.queue.clear();
                        self.cancel.store(true, Ordering::Relaxed);
                    }
                });
            });
            ui.add(egui::ProgressBar::new(pct / 100.0).show_percentage());
        } else {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Idle").color(Color32::GRAY));
                if self.done_count > 0 {
                    ui.label(format!("· {} subtitle(s) created this session", self.done_count));
                }
            });
        }
        if let Some((msg, is_err)) = &self.message {
            let color = if *is_err { Color32::from_rgb(239, 68, 68) } else { Color32::from_rgb(16, 185, 129) };
            ui.label(RichText::new(msg).color(color));
        }
        ui.add_space(4.0);
    }
}

// ------------------------------------------------------------------ trees
fn video_label(v: &Video, status: Option<&str>) -> String {
    let mut s = format!("🎞 {}   {}", v.name, scan::human(v.size));
    if let Some(st) = status {
        s.push_str(&format!("   [{st}]"));
    }
    s
}

fn draw_without(ui: &mut egui::Ui, f: &Folder, app: &App, actions: &mut Vec<Action>, root: bool) {
    let body = |ui: &mut egui::Ui, actions: &mut Vec<Action>| {
        for sub in &f.folders {
            draw_without(ui, sub, app, actions, false);
        }
        for v in &f.videos {
            ui.horizontal(|ui| {
                let mut checked = app.selected.contains(&v.path);
                let status = app.status_of(&v.path);
                if ui.add_enabled(status.is_none(), egui::Checkbox::without_text(&mut checked)).changed() {
                    actions.push(Action::Toggle(v.path.clone()));
                }
                let r = ui.selectable_label(checked, video_label(v, status)).on_hover_text("Double-click to play in VLC");
                if r.double_clicked() {
                    actions.push(Action::Play(v.path.clone(), None));
                } else if r.clicked() && status.is_none() {
                    actions.push(Action::Toggle(v.path.clone()));
                }
                r.context_menu(|ui| {
                    if ui.button("Play in VLC").clicked() {
                        actions.push(Action::Play(v.path.clone(), None));
                        ui.close();
                    }
                    if ui.button("Generate subtitle").clicked() {
                        actions.push(Action::Generate(vec![v.path.clone()]));
                        ui.close();
                    }
                    if ui.button("Show in Explorer").clicked() {
                        actions.push(Action::Reveal(v.path.clone()));
                        ui.close();
                    }
                });
            });
        }
    };
    if root {
        body(ui, actions);
    } else {
        let r = egui::CollapsingHeader::new(format!("📁 {}  ({})", f.name, f.count()))
            .id_salt(("without", &f.path))
            .default_open(true)
            .show(ui, |ui| body(ui, actions));
        r.header_response.context_menu(|ui| {
            let mut vids = Vec::new();
            f.all_videos(&mut vids);
            let paths: Vec<PathBuf> = vids.iter().map(|v| v.path.clone()).collect();
            if ui.button("Select all in this folder").clicked() {
                actions.push(Action::SelectAll(paths.clone(), true));
                ui.close();
            }
            if ui.button("Generate subtitles for this folder").clicked() {
                actions.push(Action::Generate(paths));
                ui.close();
            }
            if ui.button("Show in Explorer").clicked() {
                actions.push(Action::Reveal(f.path.clone()));
                ui.close();
            }
        });
    }
}

fn draw_with(ui: &mut egui::Ui, f: &Folder, app: &App, actions: &mut Vec<Action>, root: bool) {
    let body = |ui: &mut egui::Ui, actions: &mut Vec<Action>| {
        for sub in &f.folders {
            draw_with(ui, sub, app, actions, false);
        }
        for v in &f.videos {
            let status = app.status_of(&v.path);
            let r = ui
                .selectable_label(false, video_label(v, status))
                .on_hover_text("Double-click to play in VLC with its subtitle");
            if r.double_clicked() {
                actions.push(Action::Play(v.path.clone(), v.srts.first().cloned()));
            }
            r.context_menu(|ui| {
                if ui.button("Play with subtitle").clicked() {
                    actions.push(Action::Play(v.path.clone(), v.srts.first().cloned()));
                    ui.close();
                }
                if ui.button("Play without subtitle").clicked() {
                    actions.push(Action::Play(v.path.clone(), None));
                    ui.close();
                }
                if ui.button("Generate subtitle again (overwrite)").clicked() {
                    actions.push(Action::Generate(vec![v.path.clone()]));
                    ui.close();
                }
                if ui.button("Show in Explorer").clicked() {
                    actions.push(Action::Reveal(v.path.clone()));
                    ui.close();
                }
            });
            ui.indent(("srts", &v.path), |ui| {
                for s in &v.srts {
                    let name = s.file_name().unwrap_or_default().to_string_lossy();
                    let r = ui
                        .selectable_label(false, RichText::new(format!("📄 {name}")).color(Color32::from_rgb(16, 185, 129)))
                        .on_hover_text("Double-click to play the video with this subtitle in VLC");
                    if r.double_clicked() {
                        actions.push(Action::Play(v.path.clone(), Some(s.clone())));
                    }
                    r.context_menu(|ui| {
                        if ui.button("Play video with this subtitle").clicked() {
                            actions.push(Action::Play(v.path.clone(), Some(s.clone())));
                            ui.close();
                        }
                        if ui.button("Show in Explorer").clicked() {
                            actions.push(Action::Reveal(s.clone()));
                            ui.close();
                        }
                    });
                }
            });
        }
    };
    if root {
        body(ui, actions);
    } else {
        let r = egui::CollapsingHeader::new(format!("📁 {}  ({})", f.name, f.count()))
            .id_salt(("with", &f.path))
            .default_open(true)
            .show(ui, |ui| body(ui, actions));
        r.header_response.context_menu(|ui| {
            if ui.button("Show in Explorer").clicked() {
                actions.push(Action::Reveal(f.path.clone()));
                ui.close();
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_events(&ctx);
        debug_screenshot(&ctx);
        if self.current.is_some() || matches!(self.conn, Conn::Connecting(_)) {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        egui::Panel::top("header").show(ui, |ui| self.header(ui));
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui));

        let mut actions = Vec::new();
        let half = ui.available_width() / 2.0;
        egui::Panel::left("without").resizable(true).default_size(half).min_size(240.0).show(ui, |ui| {
            let count = self.scan.as_ref().map(|s| s.without.count()).unwrap_or(0);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Without subtitles");
                ui.label(RichText::new(format!("{count}")).color(Color32::GRAY));
            });
            ui.horizontal(|ui| {
                let n = self.selected.len();
                let can = n > 0 && self.config().is_some();
                if ui.add_enabled(can, egui::Button::new(format!("Generate subtitles ({n})"))).clicked() {
                    let mut list: Vec<PathBuf> = self.selected.iter().cloned().collect();
                    list.sort();
                    actions.push(Action::Generate(list));
                }
                if ui.add_enabled(count > 0, egui::Button::new("Select all")).clicked() {
                    if let Some(s) = &self.scan {
                        let mut v = Vec::new();
                        s.without.all_videos(&mut v);
                        actions.push(Action::SelectAll(v.iter().map(|v| v.path.clone()).collect(), true));
                    }
                }
                if ui.add_enabled(n > 0, egui::Button::new("Clear selection")).clicked() {
                    let all: Vec<PathBuf> = self.selected.iter().cloned().collect();
                    actions.push(Action::SelectAll(all, false));
                }
            });
            ui.separator();
            egui::ScrollArea::both().id_salt("left_scroll").auto_shrink([false; 2]).show(ui, |ui| match &self.scan {
                None if self.settings.folder.is_none() => {
                    ui.label("Choose a folder with Browse… to list your videos.");
                }
                None => {
                    ui.label("Scanning…");
                }
                Some(s) if s.without.count() == 0 => {
                    ui.label(RichText::new("Every video already has a subtitle.").color(Color32::GRAY));
                }
                Some(s) => draw_without(ui, &s.without, self, &mut actions, true),
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            let count = self.scan.as_ref().map(|s| s.with.count()).unwrap_or(0);
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("With subtitles");
                ui.label(RichText::new(format!("{count}")).color(Color32::GRAY));
            });
            ui.label(RichText::new("Double-click a video or a .srt to play them together in VLC.").small().color(Color32::GRAY));
            ui.separator();
            egui::ScrollArea::both().id_salt("right_scroll").auto_shrink([false; 2]).show(ui, |ui| match &self.scan {
                Some(s) if s.with.count() > 0 => draw_with(ui, &s.with, self, &mut actions, true),
                Some(_) => {
                    ui.label(RichText::new("No video with a subtitle yet.").color(Color32::GRAY));
                }
                None => {}
            });
        });

        for a in actions {
            match a {
                Action::Play(video, srt) => {
                    if let Err(e) = player::play(self.vlc.as_deref(), &video, srt.as_deref()) {
                        self.message = Some((format!("{e:#}"), true));
                    }
                }
                Action::Reveal(p) => player::reveal(&p),
                Action::Generate(list) => self.enqueue(list),
                Action::Toggle(p) => {
                    if !self.selected.remove(&p) {
                        self.selected.insert(p);
                    }
                }
                Action::SelectAll(list, on) => {
                    for p in list {
                        if on {
                            if self.status_of(&p).is_none() {
                                self.selected.insert(p);
                            }
                        } else {
                            self.selected.remove(&p);
                        }
                    }
                }
            }
        }
    }
}

/// Developer aid: `WHISPER_CLIENT_SCREENSHOT=out.ppm` saves the window once, 3 s after start.
fn debug_screenshot(ctx: &egui::Context) {
    let Ok(path) = std::env::var("WHISPER_CLIENT_SCREENSHOT") else { return };
    static ASKED: AtomicBool = AtomicBool::new(false);
    if ASKED.load(Ordering::Relaxed) {
        return;
    }
    if ctx.input(|i| i.time) < 3.0 {
        ctx.request_repaint_after(Duration::from_millis(200));
        return;
    }
    ASKED.store(true, Ordering::Relaxed);
    ctx.request_screenshot(move |image| {
        let [w, h] = image.size;
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in &image.pixels {
            out.extend_from_slice(&[px.r(), px.g(), px.b()]);
        }
        let _ = std::fs::write(&path, out);
    });
}

// ------------------------------------------------------------------ worker
fn spawn_worker(jobs: Receiver<(String, Job)>, events: Sender<JobEvent>, cancel: Arc<AtomicBool>, ctx: egui::Context) {
    std::thread::spawn(move || {
        for (base, job) in jobs {
            let _ = events.send(JobEvent::Started);
            let result = net::process(&base, &job.video, &job.model, &job.language, &events, &cancel);
            let _ = events.send(match result {
                Ok(srt) => JobEvent::Finished { video: job.video, srt },
                Err(e) => JobEvent::Failed { video: job.video, error: format!("{e:#}") },
            });
            ctx.request_repaint();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a running server: WHISPER_TEST_SERVER=http://127.0.0.1:8899 WHISPER_TEST_DIR=<folder>
    #[test]
    #[ignore]
    fn end_to_end_against_server() {
        let base = std::env::var("WHISPER_TEST_SERVER").unwrap();
        let dir = PathBuf::from(std::env::var("WHISPER_TEST_DIR").unwrap());
        let cfg = net::config(&base).unwrap();
        assert!(cfg.models.iter().any(|m| m.id == "large-v2"));

        let s = scan::scan(&dir);
        let mut without = Vec::new();
        s.without.all_videos(&mut without);
        let mut with = Vec::new();
        s.with.all_videos(&mut with);
        println!("without: {:?}", without.iter().map(|v| &v.name).collect::<Vec<_>>());
        println!("with: {:?}", with.iter().map(|v| (&v.name, &v.srts)).collect::<Vec<_>>());
        assert_eq!(with.len(), 1);
        assert_eq!(with[0].srts.len(), 2);

        let video = without.iter().find(|v| v.name == "ep01.mp4").unwrap().path.clone();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let srt = net::process(&base, &video, "large-v2", "ja", &tx, &cancel).unwrap();
        drop(tx);
        let stages: Vec<String> = rx.iter().filter_map(|e| match e {
            JobEvent::Progress { stage, .. } => Some(stage),
            _ => None,
        }).collect();
        println!("stages: {:?}", stages.iter().collect::<std::collections::BTreeSet<_>>());
        let text = std::fs::read_to_string(&srt).unwrap();
        println!("{text}");
        assert!(text.contains("Tokyo"));
        let s2 = scan::scan(&dir);
        let mut with2 = Vec::new();
        s2.with.all_videos(&mut with2);
        assert_eq!(with2.len(), 2);
    }
}
