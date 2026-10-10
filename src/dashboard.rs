//! The look of the app plus the per-server Dashboard, Players and Settings tabs.
use crate::project::{GREEN, chip, section};
use crate::server::Server;
use eframe::egui::{self, Color32, RichText, Stroke};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
const GRAY: Color32 = Color32::from_rgb(0x80, 0x86, 0x90);

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Dashboard,
    Console,
    Players,
    Settings,
    Mods,
}

/// What a dashboard click asks the app to do.
pub enum Act {
    Start,
    Restart,
    Playit,
    Backup,
    ImportWorld,
    RestoreBackup,
    Delete,
    DeleteWorld,
}

/// Slider upper bound: the PC's RAM (rounded down to 512 MB), at least 4 GB, 16 GB if unknown.
pub fn ram_max() -> u32 {
    crate::server::system_ram_mb().map_or(16384, |t| ((t as u32) / 512 * 512).max(4096))
}

pub fn theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
        use egui::{FontId, TextStyle};
        s.text_styles.insert(TextStyle::Heading, FontId::proportional(24.0));
        s.text_styles.insert(TextStyle::Body, FontId::proportional(14.5));
        s.text_styles.insert(TextStyle::Button, FontId::proportional(14.5));
        let v = &mut s.visuals;
        let rgb = Color32::from_rgb;
        v.panel_fill = rgb(0x15, 0x17, 0x1c);
        v.window_fill = rgb(0x1c, 0x1f, 0x26);
        v.faint_bg_color = rgb(0x1f, 0x23, 0x2b);
        v.extreme_bg_color = rgb(0x10, 0x12, 0x16);
        v.window_corner_radius = 12.into();
        v.hyperlink_color = GREEN;
        v.selection.bg_fill = GREEN.gamma_multiply(0.4);
        v.selection.stroke = Stroke::new(1.0, GREEN);
        v.slider_trailing_fill = true;
        let w = &mut v.widgets;
        for (x, fill) in
            [(&mut w.inactive, rgb(0x2a, 0x2f, 0x38)), (&mut w.hovered, rgb(0x34, 0x3a, 0x45)), (&mut w.active, rgb(0x3d, 0x44, 0x50))]
        {
            x.weak_bg_fill = fill;
            x.bg_fill = fill;
        }
        for x in [&mut w.noninteractive, &mut w.inactive, &mut w.hovered, &mut w.active, &mut w.open] {
            x.corner_radius = 8.into();
        }
    });
}

/// Big filled call-to-action button, full width.
pub fn big_button(ui: &mut egui::Ui, text: &str, fill: Color32, enabled: bool) -> bool {
    let fg = if fill == GREEN { Color32::BLACK } else { Color32::WHITE };
    let b =
        egui::Button::new(RichText::new(text).size(18.0).strong().color(fg)).fill(fill).min_size(egui::vec2(ui.available_width(), 44.0));
    ui.add_enabled(enabled, b).clicked()
}

fn status(running: bool) -> RichText {
    if running { RichText::new("Running").color(GREEN) } else { RichText::new("Stopped").color(GRAY) }
}

fn hms(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}h {:02}m", s / 3600, s / 60 % 60) } else { format!("{}m {:02}s", s / 60, s % 60) }
}

pub fn open_folder(dir: &Path) {
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = crate::cmd(opener).arg(dir).spawn();
}

/// One server in the sidebar. Returns true when clicked.
pub fn server_entry(ui: &mut egui::Ui, srv: &mut Server, selected: bool) -> egui::Response {
    let running = srv.running();
    let (fill, stroke) = if selected { (GREEN.gamma_multiply(0.12), GREEN) } else { (ui.visuals().faint_bg_color, Color32::TRANSPARENT) };
    let frame = egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, stroke)).corner_radius(10.0).inner_margin(10.0);
    let r = frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(&srv.name).strong().size(15.5));
        ui.weak(format!("{:?} {}", srv.cfg.flavor, srv.cfg.mc_version));
        let players = if running { online(&srv.console.lock().unwrap()).len() } else { 0 };
        let text = if running { format!("Running, {players} online") } else { "Stopped".into() };
        ui.label(RichText::new(text).small().color(if running { GREEN } else { GRAY }));
    });
    ui.add_space(2.0);
    r.response.interact(egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Server name, status and type above the tabs.
pub fn header(ui: &mut egui::Ui, srv: &mut Server) {
    let running = srv.running();
    ui.horizontal(|ui| {
        ui.heading(RichText::new(&srv.name).strong());
        ui.label(status(running).size(16.0));
        chip(ui, &format!("{:?} {}", srv.cfg.flavor, srv.cfg.mc_version));
    });
}

pub fn dashboard(ui: &mut egui::Ui, srv: &mut Server, ip: &str, playit: &crate::playit::Playit, busy: bool, tab: &mut Tab) -> Option<Act> {
    use crate::playit::Tunnel;
    let tunnel = if playit.running() { playit.tunnel.lock().unwrap().clone() } else { Tunnel::Off };
    // while linking, the playit log holds the approval link
    let claim = playit
        .log
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|l| l.split_whitespace().find(|w| w.starts_with("https://playit.gg/claim/")).map(String::from));
    let mut act = None;
    let running = srv.running();
    let port = srv.port();
    let props = std::fs::read_to_string(srv.dir.join("server.properties")).unwrap_or_default();
    let max = props_get(&props, "max-players").unwrap_or("20".into());
    let players = if running { online(&srv.console.lock().unwrap()) } else { vec![] };
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        ui.columns(3, |c| {
            section(&mut c[0], "Status", |ui| {
                ui.label(status(running).size(26.0).strong());
                if let Some(why) = srv.crash.clone().filter(|_| !running) {
                    egui::Frame::new().fill(RED.gamma_multiply(0.25)).corner_radius(8.0).inner_margin(8.0).show(ui, |ui| {
                        ui.strong("The server stopped unexpectedly");
                        ui.label(why);
                        ui.horizontal(|ui| {
                            if ui.small_button("Show console").clicked() {
                                *tab = Tab::Console;
                            }
                            let reports = srv.dir.join("crash-reports");
                            if reports.is_dir() && ui.small_button("Crash reports").clicked() {
                                open_folder(&reports);
                            }
                        });
                    });
                }
                ui.weak(srv.uptime().map_or("Not running".into(), |d| format!("Up for {}", hms(d))));
                if running {
                    if big_button(ui, "Stop", RED, true) {
                        srv.stop();
                    }
                } else if big_button(ui, "Start", GREEN, !busy) {
                    act = Some(Act::Start);
                }
            });
            section(&mut c[1], "Address to share", |ui| {
                for (label, addr) in [("This computer", format!("localhost:{port}")), ("Same Wi-Fi / home network", format!("{ip}:{port}"))]
                {
                    ui.weak(label);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&addr).monospace().size(15.0));
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(addr.clone());
                        }
                    });
                }
                ui.weak("Over the internet");
                match &tunnel {
                    Tunnel::Ready(addr) => {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(addr).monospace().size(15.0));
                            if ui.small_button("Copy").clicked() {
                                ui.ctx().copy_text(addr.clone());
                            }
                        });
                        if port != 25565 {
                            ui.colored_label(
                                ui.visuals().warn_fg_color,
                                format!("The tunnel goes to port 25565, but this server uses {port}. Set the port to 25565 in Settings."),
                            );
                        }
                    }
                    Tunnel::Waiting(msg) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak(msg);
                        });
                        if let Some(url) = claim.filter(|_| msg.starts_with("Starting")) {
                            ui.label("First time only: approve Octo Servers on playit.gg.");
                            ui.hyperlink_to("Open the approval page", url);
                        }
                    }
                    Tunnel::Off => {
                        if ui.button("Turn on playit.gg").on_hover_text("Lets friends join without port forwarding").clicked() {
                            act = Some(Act::Playit);
                        }
                    }
                }
            });
            section(&mut c[2], "Players online", |ui| {
                ui.label(RichText::new(format!("{} / {max}", players.len())).size(26.0).strong());
                ui.weak(if players.is_empty() { "Nobody online".into() } else { players.join(", ") });
                if ui.button("Manage players").clicked() {
                    *tab = Tab::Players;
                }
            });
        });
        ui.columns(3, |c| {
            section(&mut c[0], "Memory (RAM)", |ui| {
                ui.label(RichText::new(format!("{:.1} GB", srv.cfg.ram_mb as f32 / 1024.0)).size(26.0).strong());
                ui.spacing_mut().slider_width = (ui.available_width() - 90.0).max(60.0);
                if ui.add(egui::Slider::new(&mut srv.cfg.ram_mb, 1024..=ram_max()).step_by(512.0).suffix(" MB")).changed()
                    && let Err(e) = srv.save()
                {
                    crate::push(&srv.console, format!("WARN the RAM setting wasn't saved: {e}"));
                }
                match crate::server::ram_warning(srv.cfg.ram_mb) {
                    Some(w) => {
                        ui.colored_label(ui.visuals().warn_fg_color, w);
                    }
                    None => {
                        ui.weak("Applies on next start");
                    }
                }
            });
            section(&mut c[1], "Server", |ui| {
                egui::Grid::new("info").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    ui.weak("Type");
                    ui.label(format!("{:?}", srv.cfg.flavor));
                    ui.end_row();
                    ui.weak("Minecraft");
                    ui.label(&srv.cfg.mc_version);
                    ui.end_row();
                    ui.weak("Java");
                    ui.label(srv.cfg.java_major.to_string());
                    ui.end_row();
                });
            });
            section(&mut c[2], "Quick actions", |ui| {
                if ui.button("Open folder").clicked() {
                    open_folder(&srv.dir);
                }
                if ui.add_enabled(running, egui::Button::new("Restart")).clicked() {
                    act = Some(Act::Restart);
                }
                if ui.button("Console").clicked() {
                    *tab = Tab::Console;
                }
                let world = crate::server::has_world(&srv.dir);
                let r = ui.add_enabled(!running && !busy && world, egui::Button::new("Back up world"));
                if r.clicked() {
                    act = Some(Act::Backup);
                }
                r.on_disabled_hover_text(if running { "Stop the server first" } else { "No world yet" });
                let r = ui.add_enabled(!running && !busy, egui::Button::new("Import world"));
                if r.clicked() {
                    act = Some(Act::ImportWorld);
                }
                r.on_disabled_hover_text("Stop the server first");
                if srv.dir.join("backups").is_dir() {
                    if ui.button("Open backups").clicked() {
                        open_folder(&srv.dir.join("backups"));
                    }
                    let r = ui.add_enabled(!running && !busy, egui::Button::new("Restore backup"));
                    if r.clicked() {
                        act = Some(Act::RestoreBackup);
                    }
                    r.on_disabled_hover_text("Stop the server first");
                }
                let r = ui.add_enabled(!running && !busy, egui::Button::new(RichText::new("Delete server").color(RED)));
                if r.clicked() {
                    act = Some(Act::Delete);
                }
                r.on_disabled_hover_text("Stop the server first");
            });
        });
    });
    act
}

// ---------- players ----------

fn strip_ansi(s: &str) -> String {
    let mut esc = false;
    s.chars()
        .filter(|&c| {
            let keep = !esc && c != '\x1b';
            esc = if esc { !c.is_ascii_alphabetic() } else { c == '\x1b' };
            keep
        })
        .collect()
}

/// Who's online, from "<name> joined/left the game" since the last start. Same format for
/// vanilla, Paper, Fabric and Forge; chat lines (`<name> ...`) are ignored.
/// ponytail: only sees the last 5000 console lines; parse in `push` if that ever matters.
pub fn online(console: &[String]) -> Vec<String> {
    let start = console.iter().rposition(|l| l.starts_with("[octo]")).unwrap_or(0);
    let mut list: Vec<String> = vec![];
    for line in &console[start..] {
        let l = strip_ansi(line);
        let l = l.trim_end();
        let (rest, joined) = match (l.strip_suffix(" joined the game"), l.strip_suffix(" left the game")) {
            (Some(r), _) => (r, true),
            (_, Some(r)) => (r, false),
            _ => continue,
        };
        if l.contains('<') {
            continue;
        }
        let rest = rest.split(" (formerly known as ").next().unwrap_or(rest);
        let mut words = rest.rsplit(' ');
        let (name, prev) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
        if name.is_empty() || !prev.ends_with([':', ']', ')']) {
            continue;
        }
        list.retain(|n| n != name);
        if joined {
            list.push(name.into());
        }
    }
    list
}

/// `name`s from ops.json / whitelist.json / banned-players.json.
pub fn names(path: &Path) -> Vec<String> {
    let v: serde_json::Value = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    v.as_array().into_iter().flatten().filter_map(|x| x["name"].as_str()).map(String::from).collect()
}

fn name_list(ui: &mut egui::Ui, title: &str, names: &[String], action: Option<(&str, &str)>, cmd: &mut Option<String>) {
    section(ui, &format!("{title} ({})", names.len()), |ui| {
        if names.is_empty() {
            ui.weak("Nobody yet");
        }
        for n in names {
            ui.horizontal(|ui| {
                ui.label(n);
                if let Some((label, command)) = action {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(label).clicked() {
                            *cmd = Some(format!("{command} {n}"));
                        }
                    });
                }
            });
        }
    });
}

pub fn players(ui: &mut egui::Ui, srv: &mut Server, name: &mut String) {
    let running = srv.running();
    let ops = names(&srv.dir.join("ops.json"));
    let white = names(&srv.dir.join("whitelist.json"));
    let banned = names(&srv.dir.join("banned-players.json"));
    let is_op = |n: &str| ops.iter().any(|o| o.eq_ignore_ascii_case(n));
    let mut cmd = None;
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        section(ui, "Online now", |ui| {
            if !running {
                ui.weak("The server is stopped. Start it to see who's online and to change the lists below.");
                return;
            }
            let online = online(&srv.console.lock().unwrap());
            if online.is_empty() {
                ui.weak("Nobody online right now.");
            }
            for p in &online {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(p).strong().size(16.0));
                    if is_op(p) {
                        chip(ui, "op");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(RichText::new("Ban").color(RED)).clicked() {
                            cmd = Some(format!("ban {p}"));
                        }
                        if ui.button("Kick").clicked() {
                            cmd = Some(format!("kick {p}"));
                        }
                        let (label, c) = if is_op(p) { ("De-op", "deop") } else { ("Op", "op") };
                        if ui.button(label).clicked() {
                            cmd = Some(format!("{c} {p}"));
                        }
                    });
                });
            }
        });
        section(ui, "Add a player", |ui| {
            let n = name.trim().to_string();
            let ok = running && !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || "_.".contains(c));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(name).hint_text("Minecraft name").desired_width(220.0));
                if ui.add_enabled(ok, egui::Button::new("Add to whitelist")).clicked() {
                    cmd = Some(format!("whitelist add {n}"));
                }
                if ui.add_enabled(ok, egui::Button::new("Make op")).clicked() {
                    cmd = Some(format!("op {n}"));
                }
            });
            if !running {
                ui.weak("Start the server to add players.");
            }
            let props = std::fs::read_to_string(srv.dir.join("server.properties")).unwrap_or_default();
            if props_get(&props, "white-list").as_deref() != Some("true") {
                ui.weak("The whitelist is off, so anyone can join. Turn it on in the Settings tab.");
            }
        });
        ui.columns(3, |c| {
            name_list(&mut c[0], "Operators", &ops, running.then_some(("De-op", "deop")), &mut cmd);
            name_list(&mut c[1], "Whitelist", &white, running.then_some(("Remove", "whitelist remove")), &mut cmd);
            name_list(&mut c[2], "Banned", &banned, running.then_some(("Unban", "pardon")), &mut cmd);
        });
    });
    if let Some(c) = cmd {
        if c.starts_with("whitelist add") || c.starts_with("op ") {
            name.clear();
        }
        srv.send(&c);
    }
}

// ---------- server.properties ----------

fn unescape(s: &str) -> String {
    let (mut u, mut it) = (Vec::<u16>::new(), s.chars());
    while let Some(c) = it.next() {
        let c = match c {
            '\\' => match it.next() {
                Some('u') => {
                    let hex: String = it.by_ref().take(4).collect();
                    u.extend(u16::from_str_radix(&hex, 16).ok());
                    continue;
                }
                Some('n') => '\n',
                Some('t') => '\t',
                Some(x) => x,
                None => break,
            },
            c => c,
        };
        u.extend_from_slice(c.encode_utf16(&mut [0; 2]));
    }
    String::from_utf16_lossy(&u)
}

fn escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\\' => "\\\\".into(),
            c if c.is_ascii() => c.to_string(),
            c => c.encode_utf16(&mut [0; 2]).iter().map(|u| format!("\\u{u:04X}")).collect(),
        })
        .collect()
}

fn key_of(line: &str) -> Option<&str> {
    let (k, _) = line.split_once('=')?;
    (!line.trim_start().starts_with(['#', '!'])).then(|| k.trim())
}

pub fn props_get(text: &str, key: &str) -> Option<String> {
    text.lines().rev().find(|l| key_of(l) == Some(key)).map(|l| unescape(l.split_once('=').unwrap().1.trim_start()))
}

/// Rewrites only the edited keys; comments, order and unknown keys stay. Missing keys are appended.
pub fn props_set(text: &str, edits: &[(&str, &str)]) -> String {
    let mut done = vec![false; edits.len()];
    let mut out = String::new();
    for l in text.lines() {
        match key_of(l).and_then(|k| edits.iter().position(|e| e.0 == k)) {
            Some(i) => {
                out += &format!("{}={}\n", edits[i].0, escape(edits[i].1));
                done[i] = true;
            }
            None => {
                out += l;
                out.push('\n');
            }
        }
    }
    for (i, (k, v)) in edits.iter().enumerate() {
        if !done[i] {
            out += &format!("{k}={}\n", escape(v));
        }
    }
    out
}

/// Keys the Settings tab edits, with Minecraft's defaults (used when the file or key is missing).
const FIELDS: [(&str, &str); 17] = [
    ("motd", "A Minecraft Server"),
    ("level-seed", ""),
    ("difficulty", "easy"),
    ("gamemode", "survival"),
    ("max-players", "20"),
    ("server-port", "25565"),
    ("spawn-protection", "16"),
    ("view-distance", "10"),
    ("simulation-distance", "10"),
    ("pvp", "true"),
    ("online-mode", "true"),
    ("white-list", "false"),
    ("allow-flight", "false"),
    ("enable-command-block", "false"),
    ("spawn-monsters", "true"),
    ("allow-nether", "true"),
    ("hardcore", "false"),
];

/// Settings tab state: values being edited for one server.
pub struct Props {
    pub dir: PathBuf,
    vals: Vec<String>,
    orig: Vec<String>,
    msg: String,
}

impl Props {
    pub fn load(dir: &Path) -> Self {
        let text = std::fs::read_to_string(dir.join("server.properties")).unwrap_or_default();
        let vals: Vec<String> = FIELDS.iter().map(|(k, d)| props_get(&text, k).unwrap_or(d.to_string())).collect();
        Props { dir: dir.into(), orig: vals.clone(), vals, msg: String::new() }
    }

    fn val(&mut self, key: &str) -> &mut String {
        &mut self.vals[FIELDS.iter().position(|f| f.0 == key).unwrap()]
    }

    fn save(&mut self) {
        let path = self.dir.join("server.properties");
        let edits: Vec<(&str, &str)> =
            FIELDS.iter().zip(self.vals.iter().zip(&self.orig)).filter(|(_, (v, o))| v != o).map(|(f, (v, _))| (f.0, v.as_str())).collect();
        // Re-read: the server may have (re)written the file since we loaded it.
        let text = props_set(&std::fs::read_to_string(&path).unwrap_or_default(), &edits);
        let n = edits.len();
        match crate::write_atomic(&path, text) {
            Ok(()) => {
                *self = Props::load(&self.dir.clone());
                self.msg = format!("Saved {n} change{}.", if n == 1 { "" } else { "s" });
            }
            Err(e) => self.msg = format!("Couldn't save: {e}"),
        }
    }
}

fn check(ui: &mut egui::Ui, p: &mut Props, key: &str, label: &str, tip: &str) {
    let v = p.val(key);
    let mut b = v == "true";
    if ui.checkbox(&mut b, label).on_hover_text(tip).changed() {
        *v = b.to_string();
    }
}

fn combo(ui: &mut egui::Ui, p: &mut Props, key: &str, opts: &[&str]) {
    let v = p.val(key);
    // Old servers (before 1.13) store these as numbers.
    let numeric = v.parse::<usize>().ok();
    let shown = numeric.and_then(|n| opts.get(n).copied()).unwrap_or(v.as_str()).to_string();
    egui::ComboBox::from_id_salt(key).selected_text(&shown).show_ui(ui, |ui| {
        for (i, o) in opts.iter().enumerate() {
            if ui.selectable_label(shown == *o, *o).clicked() {
                *v = if numeric.is_some() { i.to_string() } else { o.to_string() };
            }
        }
    });
}

fn num(ui: &mut egui::Ui, p: &mut Props, key: &str, range: RangeInclusive<i64>) {
    let v = p.val(key);
    let mut n: i64 = v.trim().parse().unwrap_or(*range.start());
    if ui.add(egui::Slider::new(&mut n, range).clamping(egui::SliderClamping::Edits)).changed() {
        *v = n.to_string();
    }
}

fn row(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.label(label);
    body(ui);
    ui.end_row();
}

pub fn settings(ui: &mut egui::Ui, p: &mut Props, running: bool, busy: bool) -> Option<Act> {
    let mut act = None;
    if p.vals == p.orig {
        // nothing edited: follow the file (the server writes it on first start)
        let msg = std::mem::take(&mut p.msg);
        *p = Props::load(&p.dir.clone());
        p.msg = msg;
    }
    let exists = p.dir.join("server.properties").exists();
    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        if !exists {
            section(ui, "Not created yet", |ui| {
                ui.label("server.properties appears the first time the server starts. You can choose settings now; Save creates the file and Minecraft fills in the rest.");
            });
        }
        ui.columns(2, |c| {
            section(&mut c[0], "World", |ui| {
                egui::Grid::new("world").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    row(ui, "Server message (MOTD)", |ui| {
                        ui.add(egui::TextEdit::singleline(p.val("motd")).desired_width(240.0));
                    });
                    row(ui, "World seed", |ui| {
                        ui.add(egui::TextEdit::singleline(p.val("level-seed")).hint_text("random").desired_width(240.0));
                    });
                    row(ui, "Difficulty", |ui| combo(ui, p, "difficulty", &["peaceful", "easy", "normal", "hard"]));
                    row(ui, "Game mode", |ui| combo(ui, p, "gamemode", &["survival", "creative", "adventure", "spectator"]));
                });
                ui.weak("A new seed only applies to a newly generated world.");
                let world = crate::server::has_world(&p.dir);
                let r = ui.add_enabled(!running && !busy && world, egui::Button::new(RichText::new("Delete world").color(RED)));
                if r.clicked() {
                    act = Some(Act::DeleteWorld);
                }
                r.on_disabled_hover_text(if running { "Stop the server first" } else { "No world yet" });
            });
            section(&mut c[0], "Players & network", |ui| {
                egui::Grid::new("net").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    row(ui, "Max players", |ui| num(ui, p, "max-players", 1..=100));
                    row(ui, "Spawn protection", |ui| num(ui, p, "spawn-protection", 0..=64));
                    row(ui, "Port", |ui| {
                        let v = p.val("server-port");
                        let mut n: u16 = v.trim().parse().unwrap_or(25565);
                        if ui.add(egui::DragValue::new(&mut n).range(1..=65535)).changed() {
                            *v = n.to_string();
                        }
                    });
                });
            });
            section(&mut c[1], "Performance", |ui| {
                egui::Grid::new("perf").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    row(ui, "View distance", |ui| num(ui, p, "view-distance", 2..=32));
                    row(ui, "Simulation distance", |ui| num(ui, p, "simulation-distance", 2..=32));
                });
                ui.weak("Lower values use less CPU and RAM.");
            });
            section(&mut c[1], "Rules", |ui| {
                check(ui, p, "pvp", "Players can hurt each other (PvP)", "pvp");
                check(ui, p, "spawn-monsters", "Monsters spawn", "spawn-monsters");
                check(ui, p, "allow-nether", "Allow the Nether", "allow-nether");
                check(ui, p, "hardcore", "Hardcore (one life)", "hardcore");
                check(ui, p, "allow-flight", "Allow flying (needed by some mods)", "allow-flight");
                check(ui, p, "enable-command-block", "Command blocks", "enable-command-block");
                check(ui, p, "white-list", "Whitelist: only listed players can join", "white-list");
                check(ui, p, "online-mode", "Check Minecraft accounts (online mode)", "Leave this on unless you know you need it off.");
            });
        });
        let dirty = p.vals != p.orig || !exists;
        ui.horizontal(|ui| {
            let b = egui::Button::new(RichText::new("Save").size(16.0).strong().color(Color32::BLACK)).fill(GREEN).min_size(egui::vec2(120.0, 36.0));
            if ui.add_enabled(dirty, b).clicked() {
                p.save();
            }
            if ui.add_enabled(p.vals != p.orig, egui::Button::new("Undo changes")).clicked() {
                p.vals = p.orig.clone();
            }
            ui.label(&p.msg);
        });
        if running {
            ui.colored_label(ui.visuals().warn_fg_color, "The server is running: restart it to apply saved changes.");
        }
    });
    act
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn properties_round_trip() {
        let text = "#Minecraft server properties\n#Mon Sep 30\nmy-plugin-key=x\nmotd=Hi \\u00A7aGreen\nmax-players=20\n";
        assert_eq!(props_get(text, "motd").unwrap(), "Hi §aGreen");
        assert_eq!(props_set(text, &[]), text);
        let out = props_set(text, &[("max-players", "8"), ("pvp", "false")]);
        assert_eq!(out, "#Minecraft server properties\n#Mon Sep 30\nmy-plugin-key=x\nmotd=Hi \\u00A7aGreen\nmax-players=8\npvp=false\n");
        let out = props_set(text, &[("motd", "Héllo \\o/")]);
        assert_eq!(props_get(&out, "motd").unwrap(), "Héllo \\o/");
        assert_eq!(out.lines().nth(3), Some("motd=H\\u00E9llo \\\\o/"));
    }

    #[test]
    fn players_from_console() {
        let lines: Vec<String> = [
            "[octo] java -Xmx4096M -jar server.jar",
            "[12:00:00] [Server thread/INFO]: Steve joined the game",
            "[12:00:01 INFO]: \x1b[33;1mAlex joined the game\x1b[m",
            "[12:00:02] [Server thread/INFO] (Minecraft) Fab_1 joined the game",
            "[12:00:03] [Server thread/INFO] [minecraft/MinecraftServer]: Forgey joined the game",
            "[12:00:04] [Server thread/INFO]: <Steve> Notch joined the game",
            "[12:00:05] [Server thread/INFO]: New (formerly known as Old) joined the game",
            "[12:00:06] [Server thread/INFO]: Alex left the game",
        ]
        .map(String::from)
        .into();
        assert_eq!(online(&lines), ["Steve", "Fab_1", "Forgey", "New"]);
        let mut restarted = lines.clone();
        restarted.push("[octo] server exited (exit status: 0)".into());
        assert!(online(&restarted).is_empty());
    }
}
