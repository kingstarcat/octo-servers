#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
mod dashboard;
mod flavors;
mod java;
mod packs;
mod playit;
mod project;
mod server;
mod sources;
mod update;
mod worlds;

use dashboard::{Act, Tab};
use eframe::egui;
use flavors::Flavor;
use server::Server;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ---------- shared helpers ----------

pub type Log = Arc<Mutex<Vec<String>>>;

pub fn push(log: &Log, line: impl Into<String>) {
    let mut l = log.lock().unwrap();
    l.push(line.into());
    if l.len() > 5000 {
        l.drain(..1000);
    }
}

pub fn s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

pub fn data_dir() -> PathBuf {
    // OCTO_DATA_DIR lets tests (or a portable install) use another folder.
    std::env::var_os("OCTO_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| ".".into()).join("octo-servers"))
}

/// Command that never pops up a console window on Windows.
pub fn cmd(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000); // CREATE_NO_WINDOW
    c
}

pub const UA: &str = concat!("octo-servers/", env!("CARGO_PKG_VERSION"));

pub fn get_text(url: &str) -> Result<String, String> {
    ureq::get(url)
        .header("User-Agent", UA)
        .call()
        .map_err(|e| format!("{url}: {e}"))?
        .body_mut()
        .with_config()
        .limit(50 << 20)
        .read_to_string()
        .map_err(s)
}

pub fn get_json(url: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(&get_text(url)?).map_err(|e| format!("{url}: {e}"))
}

/// One entry in the live downloads panel.
pub struct Dl {
    pub name: String,
    pub done: u64,
    pub total: u64,
    pub start: Instant,
    pub end: Option<Instant>,
    pub failed: bool,
}

/// Every download of the current task (cleared when a task starts); the GUI renders it.
pub static DOWNLOADS: Mutex<Vec<Dl>> = Mutex::new(Vec::new());
/// Files waiting in `download_all`'s queue, for the overall progress bar.
pub static QUEUED: AtomicUsize = AtomicUsize::new(0);
/// How many files `download_all` fetches at once (Settings).
pub static PARALLEL: AtomicUsize = AtomicUsize::new(4);

pub fn download(url: &str, to: &Path) -> Result<(), String> {
    let name = to.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let id = {
        let mut d = DOWNLOADS.lock().unwrap();
        d.push(Dl { name, done: 0, total: 0, start: Instant::now(), end: None, failed: false });
        d.len() - 1
    };
    let set = |f: &dyn Fn(&mut Dl)| {
        if let Some(x) = DOWNLOADS.lock().unwrap().get_mut(id) {
            f(x)
        }
    };
    let r = (|| {
        let mut res = ureq::get(url).header("User-Agent", UA).call().map_err(|e| format!("{url}: {e}"))?;
        let total = res.body().content_length().unwrap_or(0);
        set(&|x| x.total = total);
        let tmp = to.with_extension("part");
        let mut f = std::fs::File::create(&tmp).map_err(s)?;
        let mut r = res.body_mut().as_reader();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = r.read(&mut buf).map_err(s)?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut f, &buf[..n]).map_err(s)?;
            set(&|x| x.done += n as u64);
        }
        drop(f);
        std::fs::rename(&tmp, to).map_err(s)
    })();
    set(&|x| {
        x.end = Some(Instant::now());
        x.failed = r.is_err();
    });
    r
}

/// Download many files, `PARALLEL` at a time. Failures are logged and counted, not fatal.
pub fn download_all(jobs: Vec<(String, PathBuf)>, log: &Log) -> usize {
    let total = jobs.len();
    QUEUED.store(total, Ordering::Relaxed);
    let queue = Mutex::new(jobs.into_iter().enumerate());
    let failed = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..PARALLEL.load(Ordering::Relaxed).max(1) {
            sc.spawn(|| {
                loop {
                    let Some((i, (url, to))) = queue.lock().unwrap().next() else { break };
                    QUEUED.fetch_sub(1, Ordering::Relaxed);
                    let name = to.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let r = to.parent().map_or(Ok(()), |p| std::fs::create_dir_all(p).map_err(s)).and_then(|_| download(&url, &to));
                    if let Err(e) = r {
                        failed.fetch_add(1, Ordering::Relaxed);
                        push(log, format!("[{}/{total}] WARN {name}: {e}", i + 1));
                    }
                }
            });
        }
    });
    failed.into_inner()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Settings {
    parallel_downloads: usize,
    backup_on_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { parallel_downloads: 4, backup_on_start: true }
    }
}

impl Settings {
    fn path() -> PathBuf {
        data_dir().join("settings.json")
    }
    fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(Self::path(), serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

/// Percent-encode a query value.
pub fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Last path segment of a URL, percent-decoded, reduced to a safe file name.
pub fn url_file_name(url: &str) -> String {
    let seg = url.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
    let b = seg.as_bytes();
    let (mut out, mut i) = (vec![], 0);
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    let name = String::from_utf8_lossy(&out).replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
    if name.is_empty() || name.starts_with('.') { format!("file{name}") } else { name }
}

fn pipe_to_log(r: impl Read + Send + 'static, log: Log) {
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines().map_while(Result::ok) {
            push(&log, line);
        }
    });
}

/// Spawn with stdout/stderr streamed into `log`. stdin is piped so callers can write to it.
pub fn spawn_logged(mut c: Command, log: &Log) -> Result<Child, String> {
    let mut child =
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("{:?}: {e}", c.get_program()))?;
    pipe_to_log(child.stdout.take().unwrap(), log.clone());
    pipe_to_log(child.stderr.take().unwrap(), log.clone());
    Ok(child)
}

pub fn run_logged(c: Command, log: &Log) -> Result<(), String> {
    let status = spawn_logged(c, log)?.wait().map_err(s)?;
    // let the reader threads flush the last lines
    std::thread::sleep(Duration::from_millis(100));
    status.success().then_some(()).ok_or(format!("exited with {status}"))
}

fn lan_ip() -> String {
    // connect() on UDP sends nothing; it just picks the outbound interface.
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|u| u.connect("8.8.8.8:80").and_then(|_| u.local_addr()))
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

// ---------- GUI ----------

struct Task {
    label: String,
    log: Log,
    handle: JoinHandle<Result<(), String>>,
}

type Versions = Arc<Mutex<Option<Result<Vec<String>, String>>>>;
/// Search results; while the search thread still holds its clone, it's loading.
/// "Check for updates" result on the Mods tab (strong count > 1 while checking).
type ModUpdates = Arc<Mutex<Option<Result<Vec<sources::ModUpdate>, String>>>>;
type Results = Arc<Mutex<Option<Result<Vec<sources::Hit>, String>>>>;

fn search(query: String, kind: &'static str, mc: Option<String>, f: Option<Flavor>) -> Results {
    spawn_search(move || sources::modrinth_search(&query, kind, mc.as_deref(), f))
}

type Searches = std::collections::HashMap<Source, (String, Results)>;

/// Keep each provider's query and results when switching to another one and back.
fn switch_search(saved: &mut Searches, from: Source, to: Source, query: &mut String, results: &mut Results) {
    if from != to {
        let (q, r) = saved.remove(&to).unwrap_or_default();
        saved.insert(from, (std::mem::replace(query, q), std::mem::replace(results, r)));
    }
}

fn spawn_search(f: impl FnOnce() -> Result<Vec<sources::Hit>, String> + Send + 'static) -> Results {
    let r = Results::default();
    let out = r.clone();
    std::thread::spawn(move || *out.lock().unwrap() = Some(f()));
    r
}

enum Pick {
    /// install/use button
    Use(String),
    /// double-clicked: open the project page
    Open(String),
}

/// Renders search hits.
fn hits(ui: &mut egui::Ui, id: &str, results: &Results, button: &str, enabled: bool) -> Option<Pick> {
    if Arc::strong_count(results) > 1 {
        ui.spinner();
        return None;
    }
    let mut picked = None;
    egui::ScrollArea::vertical().id_salt(id).auto_shrink(false).show(ui, |ui| match &*results.lock().unwrap() {
        None => {}
        Some(Err(e)) => {
            ui.colored_label(egui::Color32::RED, e);
        }
        Some(Ok(v)) if v.is_empty() => {
            ui.weak("No results.");
        }
        Some(Ok(v)) => {
            for h in v {
                let row = ui.horizontal(|ui| {
                    if !h.icon_url.is_empty() {
                        ui.add(egui::Image::new(&h.icon_url).fit_to_exact_size(egui::vec2(40.0, 40.0)).corner_radius(6.0));
                    }
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(enabled, egui::Button::new(button)).clicked() {
                                picked = Some(Pick::Use(h.slug.clone()));
                            }
                            ui.strong(&h.title);
                            // Technic and ATLauncher don't report download counts
                            if h.downloads > 0 {
                                ui.weak(format!("{} downloads", project::short(h.downloads)));
                            }
                        });
                        ui.weak(&h.description);
                    });
                });
                let r = ui.interact(row.response.rect, ui.id().with(("hit", &h.slug)), egui::Sense::click());
                if r.double_clicked() {
                    picked = Some(Pick::Open(h.slug.clone()));
                }
                r.on_hover_text("Double-click for details");
                ui.add_space(4.0);
            }
        }
    });
    picked
}

struct NewDialog {
    name: String,
    flavor: Flavor,
    versions: Versions,
    version: String,
    ram_mb: u32,
    eula: bool,
    src: Source,
    // modpack / server-files import
    link: String,
    query: String,
    results: Results,
    /// other providers' query and results, kept while their tab isn't shown
    searches: Searches,
    over_flavor: Option<Flavor>,
    over_mc: String,
    // from a world: launcher worlds (scanned when the tab opens) and the pick
    found: Option<Vec<worlds::Found>>,
    world: Option<PathBuf>,
    world_mods: bool,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum Source {
    Blank,
    Pack,
    World,
    Modrinth,
    CurseForge,
    Technic,
    ATLauncher,
}

impl Source {
    fn is_pack(self) -> bool {
        matches!(self, Self::Pack | Self::Modrinth | Self::CurseForge | Self::Technic | Self::ATLauncher)
    }
}

impl NewDialog {
    fn new() -> Self {
        let mut d = NewDialog {
            name: String::new(),
            flavor: Flavor::Paper,
            versions: Default::default(),
            version: String::new(),
            ram_mb: 4096,
            eula: false,
            src: Source::Blank,
            link: String::new(),
            query: String::new(),
            results: Default::default(),
            searches: Default::default(),
            over_flavor: None,
            over_mc: String::new(),
            found: None,
            world: None,
            world_mods: true,
        };
        d.fetch();
        d
    }

    fn fetch(&mut self) {
        self.versions = Default::default();
        self.version.clear();
        let (out, f) = (self.versions.clone(), self.flavor);
        std::thread::spawn(move || *out.lock().unwrap() = Some(flavors::versions(f)));
    }
}

struct App {
    servers: Vec<Server>,
    sel: usize,
    input: String,
    new: Option<NewDialog>,
    task: Option<Task>,
    last_log: Log,
    error: Option<String>,
    playit: playit::Playit,
    show_playit: bool,
    ip: String,
    tab: Tab,
    props: Option<dashboard::Props>,
    player_name: String,
    mod_query: String,
    mod_link: String,
    mod_results: Results,
    page: Option<project::Page>,
    cf_search: Option<browser::Search>,
    mod_source: Source,
    mod_searches: Searches,
    settings: Settings,
    show_settings: bool,
    /// start this server once the running task (the pre-start backup) succeeds
    start_after: Option<String>,
    confirm_delete: Option<String>,
    /// server name, and the worlds found on this PC
    confirm_import: Option<(String, Vec<worlds::Found>)>,
    /// a newer Octo release (filled by the startup check), and whether it's been installed
    update: Arc<Mutex<Option<update::Release>>>,
    update_dismissed: bool,
    updated_exe: Arc<Mutex<Option<PathBuf>>>,
    /// Mods tab: result of "Check for updates" (strong count > 1 while checking)
    mod_updates: ModUpdates,
}

impl App {
    fn new() -> Self {
        let mut app = App {
            servers: vec![],
            sel: 0,
            input: String::new(),
            new: None,
            task: None,
            last_log: Log::default(),
            error: None,
            playit: playit::Playit::default(),
            show_playit: false,
            ip: lan_ip(),
            tab: Tab::Dashboard,
            props: None,
            player_name: String::new(),
            mod_query: String::new(),
            mod_link: String::new(),
            mod_results: Default::default(),
            page: None,
            cf_search: None,
            mod_source: Source::Modrinth,
            mod_searches: Default::default(),
            settings: Settings::load(),
            show_settings: false,
            start_after: None,
            confirm_delete: None,
            confirm_import: None,
            update: Default::default(),
            update_dismissed: false,
            updated_exe: Default::default(),
            mod_updates: Default::default(),
        };
        PARALLEL.store(app.settings.parallel_downloads, Ordering::Relaxed);
        app.reload();
        update::cleanup();
        let slot = app.update.clone();
        // quiet on failure: no network or no release yet just means no banner
        std::thread::spawn(move || *slot.lock().unwrap() = update::check().ok().flatten());
        app
    }

    /// Start, backing up the world first when that setting is on.
    fn start_server(&mut self, i: usize) {
        let srv = &self.servers[i];
        if self.settings.backup_on_start && server::has_world(&srv.dir) {
            let (dir, name) = (srv.dir.clone(), srv.name.clone());
            self.start_after = Some(name.clone());
            self.run_task(format!("Backing up {name}"), move |log| server::backup(&dir, true, log).map(|_| ()));
            return;
        }
        self.start_now(i);
    }

    fn start_now(&mut self, i: usize) {
        let srv = &mut self.servers[i];
        let _ = srv.save();
        match java::find(srv.cfg.java_major) {
            Some(j) => {
                if let Err(e) = srv.start(&j) {
                    self.error = Some(e);
                }
            }
            None => {
                let m = srv.cfg.java_major;
                self.run_task(format!("Installing Java {m}"), move |log| java::ensure(m, log).map(|_| ()));
                self.error = Some(format!("Java {m} isn't installed yet. Installing it now; press Start again when it finishes."));
            }
        }
    }

    fn run_task(&mut self, label: String, f: impl FnOnce(&Log) -> Result<(), String> + Send + 'static) {
        DOWNLOADS.lock().unwrap().clear();
        QUEUED.store(0, Ordering::Relaxed);
        let log = Log::default();
        let l = log.clone();
        let handle = std::thread::spawn(move || f(&l));
        self.task = Some(Task { label, log, handle });
    }

    fn poll_task(&mut self) {
        if self.task.as_ref().is_some_and(|t| t.handle.is_finished()) {
            let t = self.task.take().unwrap();
            let start = self.start_after.take();
            match t.handle.join().unwrap_or(Err("task panicked".into())) {
                Ok(()) => {
                    push(&t.log, format!("{}: done", t.label));
                    if let Some(i) = start.and_then(|n| self.servers.iter().position(|s| s.name == n)) {
                        self.start_now(i);
                    }
                }
                Err(e) => {
                    push(&t.log, format!("ERROR: {e}"));
                    self.error = Some(format!("{}: {e}", t.label));
                }
            }
            self.last_log = t.log;
            self.reload();
        }
    }

    /// Re-scan disk, keeping running servers' state.
    fn reload(&mut self) {
        // drop servers whose folder was deleted
        self.servers.retain(|s| s.dir.join("octo.json").exists());
        self.sel = self.sel.min(self.servers.len().saturating_sub(1));
        for s in server::load_all() {
            if !self.servers.iter().any(|o| o.name == s.name) {
                self.servers.push(s);
            }
        }
        self.servers.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

fn console(ui: &mut egui::Ui, id: &str, log: &Log) {
    egui::ScrollArea::vertical().id_salt(id).stick_to_bottom(true).auto_shrink(false).show(ui, |ui| {
        for line in log.lock().unwrap().iter() {
            match line.find("https://") {
                Some(i) => {
                    let url = line[i..].split_whitespace().next().unwrap_or("");
                    ui.horizontal(|ui| {
                        ui.monospace(&line[..i]);
                        ui.hyperlink(url);
                    });
                }
                None => {
                    ui.monospace(line);
                }
            }
        }
    });
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(250));
        self.poll_task();
        for i in 0..self.servers.len() {
            if self.servers[i].restart && !self.servers[i].running() {
                self.servers[i].restart = false;
                self.start_now(i);
            }
        }

        if let Some(search) = &mut self.cf_search {
            ctx.request_repaint_after(Duration::from_millis(50));
            if search.poll(frame) {
                self.cf_search = None;
            }
        }

        let side = egui::Frame::side_top_panel(ui.style()).inner_margin(14.0);
        egui::Panel::left("servers").resizable(true).default_size(250.0).frame(side).show(ui, |ui| {
            ui.label(egui::RichText::new("Octo Servers").size(22.0).strong());
            ui.add_space(6.0);
            let new = egui::Button::new(egui::RichText::new("New server").strong()).min_size(egui::vec2(ui.available_width(), 34.0));
            if ui.add_enabled(self.task.is_none(), new).clicked() {
                self.new = Some(NewDialog::new());
            }
            ui.add_space(6.0);
            ui.weak("YOUR SERVERS");
            egui::ScrollArea::vertical().max_height((ui.available_height() - 170.0).max(80.0)).show(ui, |ui| {
                for (i, s) in self.servers.iter_mut().enumerate() {
                    if dashboard::server_entry(ui, s, self.sel == i) {
                        self.sel = i;
                    }
                }
            });
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                if ui.button("Settings").clicked() {
                    self.show_settings = true;
                }
                ui.checkbox(&mut self.show_playit, "Show playit log");
                let on = self.playit.running();
                if on {
                    let tunnel = self.playit.tunnel.lock().unwrap().clone();
                    match tunnel {
                        playit::Tunnel::Ready(addr) => {
                            if ui.small_button(format!("Copy {addr}")).on_hover_text("Your public address").clicked() {
                                ui.ctx().copy_text(addr);
                            }
                        }
                        playit::Tunnel::Waiting(msg) => {
                            ui.weak(msg);
                        }
                        playit::Tunnel::Off => {}
                    }
                }
                if ui.selectable_label(on, "playit.gg tunnel").clicked() {
                    if on {
                        self.playit.stop();
                    } else {
                        self.playit.start();
                        self.show_playit = true;
                    }
                }
                ui.weak("Play with friends without port forwarding:");
                ui.separator();
            });
        });

        let bottom = egui::Frame::side_top_panel(ui.style()).inner_margin(12.0);
        if let Some(t) = &self.task {
            egui::Panel::bottom("task").resizable(true).min_size(260.0).frame(bottom).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.strong(&t.label);
                });
                downloads_ui(ui);
                console(ui, "task", &t.log);
            });
        } else if self.show_playit {
            egui::Panel::bottom("playit").resizable(true).min_size(150.0).frame(bottom).show(ui, |ui| {
                ui.strong("playit.gg");
                console(ui, "playit", &self.playit.log);
            });
        }

        let central = egui::Frame::central_panel(ui.style()).inner_margin(egui::Margin::symmetric(24, 18));
        egui::CentralPanel::default().frame(central).show(ui, |ui| {
            self.update_banner(ui);
            if let Some(e) = self.error.clone() {
                egui::Frame::new().fill(dashboard::RED.gamma_multiply(0.25)).corner_radius(10.0).inner_margin(10.0).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(e.clone()).color(egui::Color32::WHITE));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Dismiss").clicked() {
                                self.error = None;
                            }
                        });
                    });
                });
                ui.add_space(6.0);
            }
            let busy = self.task.is_some();
            let Some(srv) = self.servers.get_mut(self.sel) else {
                project::section(ui, "Welcome to Octo Servers", |ui| {
                    ui.label("Host your own Minecraft server in a couple of clicks.");
                    if dashboard::big_button(ui, "Create your first server", project::GREEN, !busy) {
                        self.new = Some(NewDialog::new());
                    }
                });
                if !self.last_log.lock().unwrap().is_empty() {
                    console(ui, "last", &self.last_log);
                }
                return;
            };
            let running = srv.running();
            dashboard::header(ui, srv);
            let modded = srv.cfg.flavor != Flavor::Vanilla;
            if !modded && self.tab == Tab::Mods {
                self.tab = Tab::Dashboard;
            }
            ui.horizontal(|ui| {
                let mods = if srv.cfg.flavor == Flavor::Paper { "Plugins" } else { "Mods" };
                let tabs = [
                    (Tab::Dashboard, "Dashboard"),
                    (Tab::Console, "Console"),
                    (Tab::Players, "Players"),
                    (Tab::Settings, "Settings"),
                    (Tab::Mods, mods),
                ];
                for (t, label) in tabs {
                    if t != Tab::Mods || modded {
                        ui.selectable_value(&mut self.tab, t, egui::RichText::new(label).size(15.0));
                    }
                }
            });
            ui.separator();
            ui.add_space(4.0);
            let act = match self.tab {
                Tab::Dashboard => dashboard::dashboard(ui, srv, &self.ip, &self.playit, busy, &mut self.tab),
                Tab::Players => {
                    dashboard::players(ui, srv, &mut self.player_name);
                    None
                }
                Tab::Settings => {
                    if self.props.as_ref().is_none_or(|p| p.dir != srv.dir) {
                        self.props = Some(dashboard::Props::load(&srv.dir));
                    }
                    dashboard::settings(ui, self.props.as_mut().unwrap(), running);
                    None
                }
                Tab::Mods => {
                    self.mods_ui(ui, busy, running);
                    None
                }
                Tab::Console => {
                    let resp = ui.horizontal(|ui| {
                        let r = ui.add_enabled(
                            running,
                            egui::TextEdit::singleline(&mut self.input)
                                .hint_text("Server command, for example: op YourName")
                                .desired_width(f32::INFINITY),
                        );
                        r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    });
                    if resp.inner && !self.input.is_empty() {
                        srv.send(&std::mem::take(&mut self.input));
                        resp.response.ctx.memory_mut(|m| m.request_focus(resp.response.id));
                    }
                    egui::Frame::new().fill(ui.visuals().extreme_bg_color).corner_radius(10.0).inner_margin(10.0).show(ui, |ui| {
                        console(ui, "console", &srv.console);
                    });
                    None
                }
            };
            match act {
                Some(Act::Start) => self.start_server(self.sel),
                Some(Act::Restart) => {
                    let s = &mut self.servers[self.sel];
                    s.stop();
                    s.restart = true;
                }
                Some(Act::Playit) => {
                    self.playit.start();
                    self.show_playit = true;
                }
                Some(Act::Backup) => {
                    let s = &self.servers[self.sel];
                    let (dir, name) = (s.dir.clone(), s.name.clone());
                    self.run_task(format!("Backing up {name}"), move |log| server::backup(&dir, false, log).map(|_| ()));
                }
                Some(Act::ImportWorld) => self.confirm_import = Some((self.servers[self.sel].name.clone(), worlds::scan())),
                Some(Act::Delete) => self.confirm_delete = Some(self.servers[self.sel].name.clone()),
                None => {}
            }
        });

        self.new_dialog(&ctx);
        self.page_window(&ctx);
        self.settings_window(&ctx);
        self.delete_window(&ctx);
        self.import_window(&ctx);
    }
}

impl App {
    /// Mods / Plugins tab of the selected server.
    fn mods_ui(&mut self, ui: &mut egui::Ui, busy: bool, running: bool) {
        let srv = &self.servers[self.sel];
        let (dir, mc, flavor) = (srv.dir.clone(), srv.cfg.mc_version.clone(), srv.cfg.flavor);
        let mut add = None;
        ui.horizontal(|ui| {
            let before = self.mod_source;
            ui.selectable_value(&mut self.mod_source, Source::Modrinth, "Modrinth");
            ui.selectable_value(&mut self.mod_source, Source::CurseForge, "CurseForge");
            switch_search(&mut self.mod_searches, before, self.mod_source, &mut self.mod_query, &mut self.mod_results);
        });
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.mod_query).hint_text("Search Modrinth/CurseForge"));
            if ui.button("Search").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                if self.mod_source == Source::CurseForge {
                    self.mod_results = Default::default();
                    let class = if flavor == Flavor::Paper { "bukkit-plugins" } else { "mc-mods" };
                    self.cf_search = Some(browser::Search::new(&self.mod_query, class, Some(&mc), Some(flavor), self.mod_results.clone()));
                } else {
                    self.mod_results = search(self.mod_query.clone(), "mod", Some(mc.clone()), Some(flavor));
                }
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.mod_link)
                    .hint_text("Or paste a CurseForge or Modrinth link, or a .jar URL")
                    .desired_width(420.0),
            );
            if ui.add_enabled(!busy && !self.mod_link.trim().is_empty(), egui::Button::new("Add")).clicked() {
                add = Some(std::mem::take(&mut self.mod_link));
            }
        });
        if running {
            ui.weak("Restart the server to load added or removed mods.");
        }
        let folder = dir.join(sources::content_dir(flavor));
        // updates for installed mods, matched on Modrinth by file fingerprint
        let mut apply: Option<Vec<sources::ModUpdate>> = None;
        ui.horizontal(|ui| {
            let checking = Arc::strong_count(&self.mod_updates) > 1;
            if ui.add_enabled(!busy && !checking, egui::Button::new("Check for updates")).clicked() {
                self.mod_updates = Default::default();
                let (out, folder, mc) = (self.mod_updates.clone(), folder.clone(), mc.clone());
                std::thread::spawn(move || *out.lock().unwrap() = Some(sources::check_mod_updates(&folder, &mc, flavor)));
            }
            if checking {
                ui.spinner();
            }
            match &*self.mod_updates.lock().unwrap() {
                Some(Ok(u)) if u.is_empty() => {
                    ui.weak("Everything Modrinth knows about is up to date.");
                }
                Some(Ok(u)) => {
                    ui.label(format!("{} update(s) available", u.len()));
                    if ui.add_enabled(!busy, egui::Button::new("Update all")).clicked() {
                        apply = Some(u.clone());
                    }
                }
                Some(Err(e)) => {
                    ui.colored_label(dashboard::RED, format!("Couldn't check for updates: {e}"));
                }
                None => {}
            }
        });
        if let Some(Ok(u)) = &*self.mod_updates.lock().unwrap() {
            for m in u {
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Update")).clicked() {
                        apply = Some(vec![m.clone()]);
                    }
                    ui.label(m.file.file_name().unwrap_or_default().to_string_lossy());
                    ui.weak(format!("new version: {}", m.new_name));
                });
            }
        }
        if let Some(list) = apply {
            self.mod_updates = Default::default();
            self.run_task(format!("Updating {} mod(s)", list.len()), move |log| {
                list.iter().try_for_each(|m| sources::apply_mod_update(m, log))
            });
        }
        ui.columns(2, |cols| {
            let site = if self.mod_source == Source::CurseForge { "CurseForge" } else { "Modrinth" };
            cols[0].strong(format!("{site} results for {flavor:?} {mc}"));
            match hits(&mut cols[0], "mod_hits", &self.mod_results, "Install", !busy) {
                Some(Pick::Use(slug)) => add = Some(slug),
                Some(Pick::Open(slug)) => {
                    if sources::parse_cf(&slug).is_some() {
                        cols[0].ctx().open_url(egui::OpenUrl::new_tab(slug));
                    } else {
                        let target = project::Target::Server { dir: dir.clone(), mc: mc.clone(), flavor };
                        self.page = Some(project::Page::open(slug, target));
                    }
                }
                None => {}
            }
            cols[1].strong("Installed");
            let mut files: Vec<_> =
                std::fs::read_dir(&folder).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
            files.sort();
            egui::ScrollArea::vertical().id_salt("installed").auto_shrink(false).show(&mut cols[1], |ui| {
                for f in files {
                    ui.horizontal(|ui| {
                        if ui.small_button("Delete").clicked() {
                            let _ = std::fs::remove_file(&f);
                        }
                        ui.label(f.file_name().unwrap_or_default().to_string_lossy());
                    });
                }
            });
        });
        if let Some(src) = add {
            self.run_task(format!("Adding {src}"), move |log| sources::add_mod(&src, &mc, flavor, &dir, log));
        }
    }
}

impl App {
    fn new_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.new else { return };
        let mut open = true;
        let mut create = false;
        let mut open_page = None;
        if d.src.is_pack()
            && let Some(p) = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()))
        {
            d.link = p.display().to_string();
        }
        egui::Window::new("New server").open(&mut open).collapsible(false).default_width(700.0).show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(110.0);
                    let before = d.src;
                    for (source, label) in [
                        (Source::Blank, "Custom"),
                        (Source::Pack, "Import"),
                        (Source::World, "World"),
                        (Source::Modrinth, "Modrinth"),
                        (Source::CurseForge, "CurseForge"),
                        (Source::Technic, "Technic"),
                        (Source::ATLauncher, "ATLauncher"),
                    ] {
                        ui.selectable_value(&mut d.src, source, label);
                    }
                    if d.src != before {
                        switch_search(&mut d.searches, before, d.src, &mut d.query, &mut d.results);
                        if d.src != Source::Blank && d.ram_mb < 6144 {
                            d.ram_mb = 6144;
                        }
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    ui.set_min_width(520.0);
                    ui.separator();
                    if d.src == Source::World {
                        if let Some(p) = world_picker(ui, d.found.get_or_insert_with(worlds::scan)) {
                            if d.name.is_empty() {
                                let n = p.file_stem().unwrap_or_default().to_string_lossy();
                                d.name = n
                                    .chars()
                                    .filter(|c| c.is_ascii_alphanumeric() || " -_".contains(*c))
                                    .take(40)
                                    .collect::<String>()
                                    .trim()
                                    .into();
                            }
                            d.world = Some(p);
                        }
                        if let Some(p) = &d.world {
                            ui.label(format!("World: {}", p.display()));
                        }
                        ui.checkbox(&mut d.world_mods, "Add mods and configs");
                    }
                    if d.src.is_pack() {
                        if d.src != Source::Pack {
                            ui.horizontal(|ui| {
                                let site = match d.src {
                                    Source::CurseForge => "CurseForge",
                                    Source::Technic => "Technic",
                                    Source::ATLauncher => "ATLauncher",
                                    _ => "Modrinth",
                                };
                                let r = ui.add(egui::TextEdit::singleline(&mut d.query).hint_text(format!("Search {site} modpacks")));
                                if ui.button("Search").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) {
                                    if d.src == Source::CurseForge {
                                        d.results = Default::default();
                                        self.cf_search = Some(browser::Search::new(&d.query, "modpacks", None, None, d.results.clone()));
                                    } else {
                                        let q = d.query.clone();
                                        d.results = match d.src {
                                            Source::Technic => spawn_search(move || sources::technic_search(&q)),
                                            Source::ATLauncher => spawn_search(move || sources::atl_search(&q)),
                                            _ => search(q, "modpack", None, None),
                                        };
                                    }
                                }
                            });
                            ui.allocate_ui(egui::vec2(ui.available_width(), 220.0), |ui| {
                                match hits(ui, "pack_hits", &d.results, "Use", true) {
                                    Some(Pick::Use(slug)) => {
                                        // Modrinth hits are bare slugs; the other sites give the pack's page link
                                        let link = if slug.starts_with("https://") {
                                            slug.clone()
                                        } else {
                                            format!("https://modrinth.com/modpack/{slug}")
                                        };
                                        let name = slug.rsplit('/').next().unwrap_or(&slug);
                                        use_pack(d, link, name);
                                    }
                                    Some(Pick::Open(slug)) => open_page = Some(slug),
                                    None => {}
                                }
                            });
                        }
                        ui.label("Modpack or server files:");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.link)
                                .hint_text("CurseForge, Modrinth, Technic or ATLauncher link, .zip / .mrpack URL, or file path (or drop a file here)")
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            // Native file pickers (the dialog blocks the UI while open, which is fine for a modal pick).
                            if ui.button("Browse zip...").clicked()
                                && let Some(p) = rfd::FileDialog::new().add_filter("Modpack / server files", &["zip", "mrpack"]).pick_file()
                            {
                                d.link = p.display().to_string();
                            }
                            if ui.button("Browse folder...").clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_folder()
                            {
                                d.link = p.display().to_string();
                            }
                        });
                        ui.weak(
                            "Downloaded server packs (zip or unzipped folder) work too. CurseForge links use the pack's Server Files when offered.",
                        );
                        ui.horizontal(|ui| {
                            ui.label("Type");
                            let name = d.over_flavor.map_or("Auto-detect".into(), |f| format!("{f:?}"));
                            egui::ComboBox::from_id_salt("over").selected_text(name).show_ui(ui, |ui| {
                                ui.selectable_value(&mut d.over_flavor, None, "Auto-detect");
                                for f in Flavor::ALL {
                                    ui.selectable_value(&mut d.over_flavor, Some(f), format!("{f:?}"));
                                }
                            });
                            if d.over_flavor.is_some() {
                                ui.label("Minecraft");
                                ui.add(egui::TextEdit::singleline(&mut d.over_mc).hint_text("1.20.1").desired_width(80.0));
                            }
                        });
                    }
                    egui::Grid::new("new").num_columns(2).show(ui, |ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut d.name);
                        ui.end_row();
                        if d.src != Source::Blank {
                            ui.label("RAM");
                            ui.vertical(|ui| {
                                ui.add(egui::Slider::new(&mut d.ram_mb, 1024..=dashboard::ram_max()).step_by(512.0).suffix(" MB"));
                                if let Some(w) = server::ram_warning(d.ram_mb) {
                                    ui.colored_label(ui.visuals().warn_fg_color, w);
                                }
                            });
                            ui.end_row();
                            return;
                        }
                        ui.label("Type");
                        let before = d.flavor;
                        egui::ComboBox::from_id_salt("flavor").selected_text(format!("{:?}", d.flavor)).show_ui(ui, |ui| {
                            for f in Flavor::ALL {
                                ui.selectable_value(&mut d.flavor, f, format!("{f:?}"));
                            }
                        });
                        if d.flavor != before {
                            d.fetch();
                        }
                        ui.end_row();
                        ui.label("Version");
                        match &*d.versions.lock().unwrap() {
                            None => {
                                ui.spinner();
                            }
                            Some(Err(e)) => {
                                ui.colored_label(egui::Color32::RED, e);
                            }
                            Some(Ok(vs)) => {
                                if d.version.is_empty() {
                                    d.version = vs.first().cloned().unwrap_or_default();
                                }
                                egui::ComboBox::from_id_salt("ver").selected_text(&d.version).height(400.0).show_ui(ui, |ui| {
                                    for v in vs {
                                        ui.selectable_value(&mut d.version, v.clone(), v);
                                    }
                                });
                            }
                        }
                        ui.end_row();
                        ui.label("RAM");
                        ui.vertical(|ui| {
                            ui.add(egui::Slider::new(&mut d.ram_mb, 1024..=dashboard::ram_max()).step_by(512.0).suffix(" MB"));
                            if let Some(w) = server::ram_warning(d.ram_mb) {
                                ui.colored_label(ui.visuals().warn_fg_color, w);
                            }
                        });
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut d.eula, "I agree to the");
                        ui.hyperlink_to("Minecraft EULA", "https://aka.ms/MinecraftEULA");
                    });
                    let err = server::validate_name(&d.name).err();
                    if let Some(e) = &err {
                        ui.weak(e);
                    }
                    let ready = match d.src {
                        Source::World => d.world.is_some(),
                        Source::Blank => !d.version.is_empty(),
                        _ => !d.link.trim().is_empty() && (d.over_flavor.is_none() || !d.over_mc.trim().is_empty()),
                    };
                    let ok = err.is_none() && d.eula && ready;
                    create = ui.add_enabled(ok, egui::Button::new("Create")).clicked();
                });
            });
        });
        if let Some(slug) = open_page {
            if slug.starts_with("https://") {
                ctx.open_url(egui::OpenUrl::new_tab(slug));
            } else {
                self.page = Some(project::Page::open(slug, project::Target::Modpack));
            }
        }
        if create {
            let d = self.new.take().unwrap();
            let name = d.name.trim().to_string();
            if let Some(src) = d.world.clone().filter(|_| d.src == Source::World) {
                let (ram, with_mods) = (d.ram_mb, d.world_mods);
                self.run_task(format!("Creating {name} from a world"), move |log| {
                    server::create_with(&name, ram, log, |dir, log| worlds::import(&src, dir, with_mods, log))?;
                    worlds::test_start(&server::servers_dir().join(&name), log)
                });
                return;
            }
            if d.src.is_pack() {
                let (src, ram) = (d.link.trim().to_string(), d.ram_mb);
                let over = d.over_flavor.map(|f| (f, d.over_mc.trim().to_string()));
                self.run_task(format!("Importing {name}"), move |log| {
                    server::create_with(&name, ram, log, |dir, log| packs::import(&src, dir, over, log))
                });
                return;
            }
            let cfg = server::Config { flavor: d.flavor, mc_version: d.version, java_major: 0, ram_mb: d.ram_mb, launch: None };
            self.run_task(format!("Creating {name}"), move |log| server::create(&name, cfg, log));
        } else if !open {
            self.new = None;
        }
    }
}

fn use_pack(d: &mut NewDialog, link: String, slug: &str) {
    d.link = link;
    if !d.src.is_pack() {
        d.src = Source::Pack;
    }
    if d.name.is_empty() {
        d.name = slug.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(40).collect();
    }
}

/// Worlds found on this PC, and pickers for ones elsewhere. Returns the picked world.
fn world_picker(ui: &mut egui::Ui, found: &[worlds::Found]) -> Option<PathBuf> {
    let mut pick = None;
    if found.is_empty() {
        ui.weak("No worlds found in the Minecraft launcher, CurseForge, Prism, Modrinth, ATLauncher, GDLauncher, FTB or Technic folders.");
    } else {
        egui::ScrollArea::vertical().id_salt("worlds").max_height(220.0).show(ui, |ui| {
            egui::Grid::new("worlds").num_columns(3).striped(true).show(ui, |ui| {
                for w in found {
                    ui.label(&w.name);
                    ui.weak(&w.place);
                    if ui.button("Use").clicked() {
                        pick = Some(w.dir.clone());
                    }
                    ui.end_row();
                }
            });
        });
    }
    ui.horizontal(|ui| {
        if ui.button("Choose folder...").clicked() {
            pick = rfd::FileDialog::new().pick_folder();
        }
        if ui.button("Choose zip...").clicked() {
            pick = rfd::FileDialog::new().add_filter("World zip", &["zip"]).pick_file();
        }
    });
    pick
}

fn bytes(n: f64) -> String {
    if n >= 1e9 {
        format!("{:.2} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else {
        format!("{:.0} KB", n / 1e3)
    }
}

/// Live view of the current task's downloads: overall bar + one row per active file.
fn downloads_ui(ui: &mut egui::Ui) {
    let d = DOWNLOADS.lock().unwrap();
    if d.is_empty() {
        return;
    }
    let queued = QUEUED.load(Ordering::Relaxed);
    let finished = d.iter().filter(|x| x.end.is_some()).count();
    let failed = d.iter().filter(|x| x.failed).count();
    let active: Vec<&Dl> = d.iter().filter(|x| x.end.is_none()).collect();
    let speed = |x: &Dl| x.done as f64 / x.start.elapsed().as_secs_f64().max(0.2);
    let total_speed: f64 = active.iter().map(|x| speed(x)).sum();
    let all = d.len() + queued;
    let got: u64 = d.iter().map(|x| x.done).sum();
    ui.add(egui::ProgressBar::new(finished as f32 / all.max(1) as f32).text(format!(
        "{finished}/{all} files, {} downloaded, {}/s{}",
        bytes(got as f64),
        bytes(total_speed),
        if failed > 0 { format!(", {failed} failed") } else { String::new() }
    )));
    egui::Grid::new("dl").num_columns(3).striped(true).show(ui, |ui| {
        for x in active {
            let frac = if x.total > 0 { x.done as f32 / x.total as f32 } else { 0.0 };
            let size = if x.total > 0 { format!("{} / {}", bytes(x.done as f64), bytes(x.total as f64)) } else { bytes(x.done as f64) };
            ui.add(egui::ProgressBar::new(frac).desired_width(220.0).text(size));
            ui.monospace(format!("{}/s", bytes(speed(x))));
            ui.label(&x.name);
            ui.end_row();
        }
    });
    ui.separator();
}

impl App {
    fn page_window(&mut self, ctx: &egui::Context) {
        let Some(page) = &mut self.page else { return };
        let mut open = true;
        let action = page.show(ctx, &mut open, self.task.is_some());
        let (slug, target) = (page.slug.clone(), page.target.clone());
        if !open {
            self.page = None;
        }
        match (action, target) {
            (Some(project::Action::UsePack(link)), _) => {
                let d = self.new.get_or_insert_with(NewDialog::new);
                use_pack(d, link, &slug);
                self.page = None;
            }
            (Some(project::Action::Install(version)), project::Target::Server { dir, mc, flavor }) => {
                self.run_task(format!("Adding {slug}"), move |log| match version {
                    Some(v) => sources::modrinth_install_version(&v, &mc, flavor, &dir.join(sources::content_dir(flavor)), log),
                    None => sources::add_mod(&slug, &mc, flavor, &dir, log),
                });
            }
            _ => {}
        }
    }

    fn update_banner(&mut self, ui: &mut egui::Ui) {
        if let Some(exe) = self.updated_exe.lock().unwrap().clone() {
            project::section(ui, "Update installed", |ui| {
                ui.label("Restart Octo Servers to use the new version. Running servers are stopped first.");
                if ui.button("Restart now").clicked() {
                    let _ = cmd(&exe).spawn();
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            return;
        }
        let Some(r) = self.update.lock().unwrap().clone().filter(|_| !self.update_dismissed) else { return };
        let mut install = false;
        project::section(ui, &format!("Octo Servers {} is available", r.version), |ui| {
            ui.horizontal(|ui| {
                install = ui.add_enabled(self.task.is_none(), egui::Button::new("Update now")).clicked();
                if ui.button("Later").clicked() {
                    self.update_dismissed = true;
                }
            });
        });
        if install {
            self.update_dismissed = true;
            let done = self.updated_exe.clone();
            self.run_task(format!("Updating to {}", r.version), move |log| {
                let exe = update::install(&r, log)?;
                *done.lock().unwrap() = Some(exe);
                Ok(())
            });
        }
    }

    fn delete_window(&mut self, ctx: &egui::Context) {
        let Some(name) = self.confirm_delete.clone() else { return };
        let (mut close, mut delete) = (false, false);
        egui::Window::new("Delete server").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
            ctx,
            |ui| {
                ui.label(format!("Delete \"{name}\"?"));
                ui.label("This permanently removes its folder, including the world and its backups.");
                ui.horizontal(|ui| {
                    delete = ui.button(egui::RichText::new("Delete").color(dashboard::RED)).clicked();
                    close = ui.button("Cancel").clicked();
                });
            },
        );
        if delete && let Some(dir) = self.servers.iter().find(|s| s.name == name).map(|s| s.dir.clone()) {
            self.run_task(format!("Deleting {name}"), move |_| {
                std::fs::remove_dir_all(&dir).map_err(|e| format!("Couldn't delete the folder: {e}"))
            });
        }
        if delete || close {
            self.confirm_delete = None;
        }
    }

    fn import_window(&mut self, ctx: &egui::Context) {
        let Some((name, found)) = &self.confirm_import else { return };
        let Some(dir) = self.servers.iter().find(|s| &s.name == name).map(|s| s.dir.clone()) else { return };
        let (mut close, mut pick) = (false, None);
        egui::Window::new("Import world").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(
            ctx,
            |ui| {
                ui.label(format!("Replace the world of \"{name}\" with a world from this PC."));
                if server::has_world(&dir) {
                    ui.label("The current world is backed up first.");
                }
                ui.weak("Only the world is copied. To bring a modded world's mods too, use New server and pick From a world.");
                pick = world_picker(ui, found);
                close = ui.button("Cancel").clicked();
            },
        );
        let name = name.clone();
        if let Some(src) = pick {
            self.run_task(format!("Importing a world into {name}"), move |log| server::import_world(&dir, &src, log));
            close = true;
        }
        if close {
            self.confirm_import = None;
        }
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let r = ui.add(egui::Slider::new(&mut self.settings.parallel_downloads, 1..=16).text("mods downloaded at once"));
            if r.changed() {
                PARALLEL.store(self.settings.parallel_downloads, Ordering::Relaxed);
                self.settings.save();
            }
            ui.weak("Higher is faster on good connections. Lower it if downloads fail or get rate-limited.");
            if ui.checkbox(&mut self.settings.backup_on_start, "Back up the world before each start").changed() {
                self.settings.save();
            }
            ui.weak("Keeps the last 5 automatic backups in each server's backups folder.");
        });
        self.show_settings = open;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Don't leave orphaned servers behind (and give them a chance to save the world).
        for s in &mut self.servers {
            s.stop();
        }
        for s in &mut self.servers {
            s.wait_or_kill(Duration::from_secs(15));
        }
        self.playit.stop();
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        // Wry child webviews require X11. Wayland desktops use XWayland.
        #[cfg(target_os = "linux")]
        event_loop_builder: Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        })),
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 560.0])
            .with_title("Octo Servers"),
        ..Default::default()
    };
    eframe::run_native(
        "Octo Servers",
        opts,
        Box::new(|cc| {
            egui_extras::install_image_loaders(&cc.egui_ctx);
            dashboard::theme(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_survives_tab_switch() {
        let (mut saved, mut query, mut results) = (Searches::default(), "atm 10".to_string(), Results::default());
        *results.lock().unwrap() = Some(Ok(vec![]));
        let atm = results.clone();
        switch_search(&mut saved, Source::CurseForge, Source::Modrinth, &mut query, &mut results);
        assert!(query.is_empty() && results.lock().unwrap().is_none(), "Modrinth starts empty");
        query = "create".into();
        switch_search(&mut saved, Source::Modrinth, Source::CurseForge, &mut query, &mut results);
        assert_eq!(query, "atm 10");
        assert!(Arc::ptr_eq(&results, &atm), "same results, including a search still running");
        switch_search(&mut saved, Source::CurseForge, Source::Modrinth, &mut query, &mut results);
        assert_eq!(query, "create");
    }

    #[test]
    fn file_names() {
        assert_eq!(url_file_name("https://x/files/1/2/All%20the%20Mods%209-1.1.1.zip?api-key=1"), "All the Mods 9-1.1.1.zip");
        assert_eq!(url_file_name("https://x/a%2F..%2Fb.jar"), "a_.._b.jar");
        assert_eq!(url_file_name("https://x/"), "file");
    }

    /// Renders the whole app with a fake server on every tab (and the dialogs) to catch panics.
    /// cargo test render_app -- --ignored
    #[test]
    #[ignore]
    fn render_app() {
        let data = std::env::temp_dir().join(format!("octo-ui-{}", std::process::id()));
        let dir = data.join("servers").join("Test");
        std::fs::create_dir_all(dir.join("plugins")).unwrap();
        std::fs::write(dir.join("octo.json"), r#"{"flavor":"Paper","mc_version":"1.21.1","java_major":21,"ram_mb":4096}"#).unwrap();
        std::fs::write(dir.join("server.properties"), "#c\nmotd=Test\nmax-players=10\ndifficulty=2\n").unwrap();
        std::fs::write(dir.join("ops.json"), r#"[{"uuid":"x","name":"Steve","level":4}]"#).unwrap();
        std::fs::write(dir.join("banned-players.json"), r#"[{"name":"Griefer"}]"#).unwrap();
        std::fs::write(dir.join("plugins").join("x.jar"), "").unwrap();
        unsafe { std::env::set_var("OCTO_DATA_DIR", &data) };
        let mut app = App::new();
        assert_eq!(app.servers.len(), 1);
        app.error = Some("Something went wrong".into());
        push(&app.servers[0].console, "[12:00:00] [Server thread/INFO]: Steve joined the game https://x");
        let ctx = egui::Context::default();
        dashboard::theme(&ctx);
        let frame = |app: &mut App| {
            let mut o = ctx.run_ui(egui::RawInput::default(), |ui| eframe::App::ui(app, ui, &mut eframe::Frame::_new_kittest()));
            o.textures_delta.clear();
            for s in &o.shapes {
                if let egui::Shape::Text(t) = &s.shape {
                    assert!(!t.galley.text().contains("Double use"), "{}", t.galley.text());
                }
            }
        };
        for t in [Tab::Dashboard, Tab::Console, Tab::Players, Tab::Settings, Tab::Mods] {
            app.tab = t;
            (0..3).for_each(|_| frame(&mut app));
        }
        // crash banner, delete confirmation, a world for the backup buttons, the settings window
        app.tab = Tab::Dashboard;
        app.servers[0].crash = Some(server::diagnose(&["java.lang.OutOfMemoryError".into()], 21, 25565));
        std::fs::create_dir_all(app.servers[0].dir.join("world")).unwrap();
        std::fs::create_dir_all(app.servers[0].dir.join("backups")).unwrap();
        app.confirm_delete = Some(app.servers[0].name.clone());
        app.show_settings = true;
        (0..3).for_each(|_| frame(&mut app));
        app.confirm_delete = None;
        app.confirm_import = Some((app.servers[0].name.clone(), worlds::scan()));
        (0..3).for_each(|_| frame(&mut app));
        app.confirm_import = None;
        app.show_settings = false;
        app.servers[0].crash = None;
        // A fake "java" that prints a join line and echoes commands until `stop` (unix only).
        #[cfg(unix)]
        {
            let java = data.join("fake-java");
            std::fs::write(
                &java,
                "#!/bin/sh\necho '[12:00:00 INFO]: Alex joined the game'\nwhile read l; do echo \"$l\"; [ \"$l\" = stop ] && exit; done\n",
            )
            .unwrap();
            std::fs::set_permissions(&java, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            app.servers[0].cfg.launch = Some(vec![]);
            app.servers[0].start(&java).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(dashboard::online(&app.servers[0].console.lock().unwrap()), ["Alex"]);
            for t in [Tab::Dashboard, Tab::Players, Tab::Settings] {
                app.tab = t;
                (0..3).for_each(|_| frame(&mut app));
            }
            app.servers[0].send("kick Alex");
            app.servers[0].stop();
            app.servers[0].wait_or_kill(Duration::from_secs(5));
            assert!(!app.servers[0].running());
            assert!(app.servers[0].console.lock().unwrap().iter().any(|l| l == "kick Alex"));
        }
        app.new = Some(NewDialog::new());
        app.show_settings = true;
        app.servers.clear();
        (0..3).for_each(|_| frame(&mut app));
        let mut d = NewDialog::new();
        d.src = Source::World;
        d.world = Some(data.join("My World"));
        app.new = Some(d);
        (0..3).for_each(|_| frame(&mut app));
        for source in [Source::Pack, Source::Modrinth, Source::CurseForge, Source::Technic, Source::ATLauncher] {
            let mut d = NewDialog::new();
            d.src = source;
            d.results = Arc::new(Mutex::new(Some(Ok(vec![sources::Hit {
                title: "Test pack".into(),
                slug: "test-pack".into(),
                description: "Test description".into(),
                downloads: 123,
                icon_url: String::new(),
            }]))));
            app.new = Some(d);
            (0..3).for_each(|_| frame(&mut app));
        }
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// The downloads panel's data: concurrency honours PARALLEL, rows get name/size/progress.
    /// cargo test parallel_downloads -- --ignored --nocapture
    #[test]
    #[ignore]
    fn parallel_downloads() {
        let dir = std::env::temp_dir().join(format!("octo-dl-{}", std::process::id()));
        let url = "https://cdn.modrinth.com/data/P7dR8mSH/versions/Mys3P7lK/fabric-api-0.116.17%2B1.21.1.jar";
        for parallel in [4, 1] {
            DOWNLOADS.lock().unwrap().clear();
            PARALLEL.store(parallel, Ordering::Relaxed);
            let jobs = (0..8).map(|i| (url.to_string(), dir.join(format!("{parallel}-{i}.jar")))).collect();
            let max_active = Arc::new(AtomicUsize::new(0));
            let (m, stop) = (max_active.clone(), Arc::new(std::sync::atomic::AtomicBool::new(false)));
            let s2 = stop.clone();
            let mon = std::thread::spawn(move || {
                while !s2.load(Ordering::Relaxed) {
                    let n = DOWNLOADS.lock().unwrap().iter().filter(|x| x.end.is_none()).count();
                    m.fetch_max(n, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            let t0 = Instant::now();
            let failed = download_all(jobs, &Log::default());
            stop.store(true, Ordering::Relaxed);
            mon.join().unwrap();
            let d = DOWNLOADS.lock().unwrap();
            println!(
                "parallel={parallel}: {} files, failed={failed}, max active={}, {:.1}s, first row: {} {}/{} bytes",
                d.len(),
                max_active.load(Ordering::Relaxed),
                t0.elapsed().as_secs_f64(),
                d[0].name,
                d[0].done,
                d[0].total
            );
            assert_eq!(failed, 0);
            assert!(max_active.load(Ordering::Relaxed) <= parallel);
            assert!(d.iter().all(|x| x.total > 0 && x.done == x.total && x.end.is_some()));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
