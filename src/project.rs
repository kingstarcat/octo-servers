//! In-app Modrinth project page (double-click a search result).
use crate::flavors::Flavor;
use crate::{enc, get_json, sources};
use eframe::egui::{self, Color32, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MR: &str = "https://api.modrinth.com/v2";
pub(crate) const GREEN: Color32 = Color32::from_rgb(0x1b, 0xd9, 0x6a);

/// What the page's install buttons do.
#[derive(Clone)]
pub enum Target {
    /// choosing a modpack in the New server dialog
    Modpack,
    /// adding a mod/plugin to this server
    Server { dir: PathBuf, mc: String, flavor: Flavor },
}

pub enum Action {
    /// modpack link (optionally pinned to a version) for the New server dialog
    UsePack(String),
    /// install this version id into the target server (None = newest compatible)
    Install(Option<String>),
}

struct Data {
    p: Value,
    versions: Vec<Value>,
    members: Vec<Value>,
    body: String,
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Description,
    Gallery,
    Changelog,
    Versions,
}

pub struct Page {
    pub slug: String,
    pub target: Target,
    data: Arc<Mutex<Option<Result<Data, String>>>>,
    tab: Tab,
    md: CommonMarkCache,
}

impl Page {
    pub fn open(slug: String, target: Target) -> Self {
        let data: Arc<Mutex<Option<Result<Data, String>>>> = Arc::default();
        let (out, s) = (Arc::clone(&data), slug.clone());
        std::thread::spawn(move || {
            let r = (|| {
                let p = get_json(&format!("{MR}/project/{}", enc(&s)))?;
                let arr = |v: Value| v.as_array().cloned().unwrap_or_default();
                let versions = arr(get_json(&format!("{MR}/project/{}/version", enc(&s)))?);
                let members = arr(get_json(&format!("{MR}/project/{}/members", enc(&s))).unwrap_or_default());
                let body = html_to_md(p["body"].as_str().unwrap_or(""));
                Ok(Data { p, versions, members, body })
            })();
            *out.lock().unwrap() = Some(r);
        });
        Page { slug, target, data, tab: Tab::Description, md: CommonMarkCache::default() }
    }

    fn compatible(&self, v: &Value) -> bool {
        match &self.target {
            Target::Modpack => true,
            Target::Server { mc, flavor, .. } => {
                let has = |k: &str, x: &str| v[k].as_array().is_some_and(|a| a.iter().any(|y| y == x));
                has("game_versions", mc) && sources::loaders(*flavor).iter().any(|l| has("loaders", l))
            }
        }
    }

    fn install_label(&self) -> &'static str {
        match self.target {
            Target::Modpack => "Use this modpack",
            Target::Server { .. } => "Install",
        }
    }

    /// Returns an action when an install button is clicked. `open` false = closed.
    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool, busy: bool) -> Option<Action> {
        let mut action = None;
        let title = format!("{} on Modrinth", self.slug);
        egui::Window::new(title).id(egui::Id::new("project_page")).open(open).default_size([1000.0, 700.0]).show(ctx, |ui| {
            let data = Arc::clone(&self.data);
            let guard = data.lock().unwrap();
            match &*guard {
                None => {
                    ui.spinner();
                }
                Some(Err(e)) => {
                    ui.colored_label(Color32::RED, e);
                }
                Some(Ok(d)) => action = self.page(ui, d, busy),
            }
        });
        action
    }

    fn page(&mut self, ui: &mut egui::Ui, d: &Data, busy: bool) -> Option<Action> {
        let p = &d.p;
        let mut action = None;
        let strs =
            |k: &str| -> Vec<String> { p[k].as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(String::from).collect() };

        // ---- header ----
        ui.horizontal(|ui| {
            if let Some(icon) = p["icon_url"].as_str() {
                ui.add(egui::Image::new(icon).fit_to_exact_size(egui::vec2(96.0, 96.0)).corner_radius(12.0));
            }
            ui.vertical(|ui| {
                ui.label(RichText::new(p["title"].as_str().unwrap_or("")).size(26.0).strong());
                ui.label(p["description"].as_str().unwrap_or(""));
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!("{} downloads", short(p["downloads"].as_u64().unwrap_or(0))));
                    ui.label(format!("{} followers", short(p["followers"].as_u64().unwrap_or(0))));
                    for c in strs("categories") {
                        chip(ui, &c);
                    }
                });
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                let compatible = d.versions.iter().any(|v| self.compatible(v));
                let btn = egui::Button::new(RichText::new(self.install_label()).size(18.0).color(Color32::BLACK)).fill(GREEN);
                let r = ui.add_enabled(!busy && compatible, btn);
                if !compatible {
                    r.on_disabled_hover_text("No version for this server's Minecraft version / type");
                } else if r.clicked() {
                    action = Some(self.default_action());
                }
            });
        });

        if p["status"] == "archived" {
            egui::Frame::group(ui.style()).fill(Color32::from_rgb(0x1e, 0x2a, 0x40)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.strong(format!("{} has been archived", p["title"].as_str().unwrap_or("")));
                ui.label("It will not receive any further updates unless the author decides to unarchive the project.");
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (t, name) in
                [(Tab::Description, "Description"), (Tab::Gallery, "Gallery"), (Tab::Changelog, "Changelog"), (Tab::Versions, "Versions")]
            {
                ui.selectable_value(&mut self.tab, t, name);
            }
        });
        ui.separator();

        // ---- sidebar ----
        egui::Panel::right("project_side").resizable(false).exact_size(270.0).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("side").show(ui, |ui| {
                section(ui, "Compatibility", |ui| {
                    ui.label("Minecraft: Java Edition");
                    let mut gv = strs("game_versions");
                    gv.reverse();
                    let more = gv.len().saturating_sub(16);
                    ui.horizontal_wrapped(|ui| {
                        for v in gv.iter().take(16) {
                            chip(ui, v);
                        }
                        if more > 0 {
                            ui.weak(format!("+{more} more"));
                        }
                    });
                    ui.label("Platforms");
                    ui.horizontal_wrapped(|ui| strs("loaders").iter().for_each(|l| chip(ui, l)));
                    ui.label("Supported environments");
                    chip(ui, &environments(p));
                });
                let tags = [strs("categories"), strs("additional_categories")].concat();
                if !tags.is_empty() {
                    section(ui, "Tags", |ui| {
                        ui.horizontal_wrapped(|ui| tags.iter().for_each(|t| chip(ui, t)));
                    });
                }
                if !d.members.is_empty() {
                    section(ui, "Creators", |ui| {
                        for m in &d.members {
                            ui.horizontal(|ui| {
                                if let Some(a) = m["user"]["avatar_url"].as_str() {
                                    ui.add(egui::Image::new(a).fit_to_exact_size(egui::vec2(24.0, 24.0)).corner_radius(12.0));
                                }
                                ui.strong(m["user"]["username"].as_str().unwrap_or("?"));
                                ui.weak(m["role"].as_str().unwrap_or(""));
                            });
                        }
                    });
                }
                section(ui, "Links", |ui| {
                    for (k, name) in [("source_url", "Source"), ("issues_url", "Issues"), ("wiki_url", "Wiki"), ("discord_url", "Discord")]
                    {
                        if let Some(u) = p[k].as_str() {
                            ui.hyperlink_to(name, u);
                        }
                    }
                    let kind = p["project_type"].as_str().unwrap_or("project");
                    ui.hyperlink_to("View on Modrinth", format!("https://modrinth.com/{kind}/{}", self.slug));
                });
            });
        });

        // ---- main content ----
        egui::ScrollArea::vertical().id_salt(("content", self.tab as u8)).auto_shrink(false).show(ui, |ui| match self.tab {
            Tab::Description => {
                CommonMarkViewer::new().max_image_width(Some(ui.available_width() as usize)).show(ui, &mut self.md, &d.body);
            }
            Tab::Gallery => {
                let g = p["gallery"].as_array().cloned().unwrap_or_default();
                if g.is_empty() {
                    ui.weak("No gallery images.");
                }
                for img in g {
                    if let Some(u) = img["url"].as_str() {
                        ui.add(egui::Image::new(u).max_width(ui.available_width()).corner_radius(8.0));
                    }
                    if let Some(t) = img["title"].as_str() {
                        ui.strong(t);
                    }
                    if let Some(t) = img["description"].as_str() {
                        ui.weak(t);
                    }
                    ui.add_space(10.0);
                }
            }
            Tab::Changelog => {
                for v in d.versions.iter().take(25) {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(v["name"].as_str().unwrap_or("")).strong().size(16.0));
                        ui.weak(date(v));
                    });
                    let cl = v["changelog"].as_str().filter(|c| !c.trim().is_empty()).unwrap_or("_No changelog._");
                    CommonMarkViewer::new().show(ui, &mut self.md, &html_to_md(cl));
                    ui.separator();
                }
            }
            Tab::Versions => {
                egui::Grid::new("versions").striped(true).num_columns(6).show(ui, |ui| {
                    for h in ["", "Name", "Game versions", "Platforms", "Published", "Downloads"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for v in &d.versions {
                        let ok = self.compatible(v);
                        let r = ui.add_enabled(!busy && ok, egui::Button::new("Install"));
                        if r.clicked() {
                            let id = v["id"].as_str().unwrap_or("").to_string();
                            action = Some(match self.target {
                                Target::Modpack => Action::UsePack(format!("https://modrinth.com/modpack/{}/version/{id}", self.slug)),
                                Target::Server { .. } => Action::Install(Some(id)),
                            });
                        }
                        ui.vertical(|ui| {
                            ui.label(v["name"].as_str().unwrap_or(""));
                            ui.weak(format!(
                                "{} ({})",
                                v["version_number"].as_str().unwrap_or(""),
                                v["version_type"].as_str().unwrap_or("")
                            ));
                        });
                        ui.label(list(&v["game_versions"], 4));
                        ui.label(list(&v["loaders"], 4));
                        ui.label(date(v));
                        ui.label(short(v["downloads"].as_u64().unwrap_or(0)));
                        ui.end_row();
                    }
                });
            }
        });
        action
    }

    fn default_action(&self) -> Action {
        match self.target {
            Target::Modpack => Action::UsePack(format!("https://modrinth.com/modpack/{}", self.slug)),
            Target::Server { .. } => Action::Install(None),
        }
    }
}

pub(crate) fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().fill(ui.visuals().faint_bg_color).corner_radius(12.0).inner_margin(14.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(title).strong().size(17.0));
        body(ui);
    });
    ui.add_space(8.0);
}

pub(crate) fn chip(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| ui.label(RichText::new(text).small()));
}

pub fn short(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}K", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn list(v: &Value, max: usize) -> String {
    let a: Vec<&str> = v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).collect();
    let mut s = a.iter().rev().take(max).copied().collect::<Vec<_>>().join(", ");
    if a.len() > max {
        s += &format!(" +{}", a.len() - max);
    }
    s
}

fn date(v: &Value) -> String {
    v["date_published"].as_str().unwrap_or("").chars().take(10).collect()
}

fn environments(p: &Value) -> String {
    let side = |k: &str| p[k].as_str().unwrap_or("unknown");
    match (side("client_side"), side("server_side")) {
        ("unsupported", _) => "Server-side".into(),
        (_, "unsupported") => "Client-side".into(),
        ("required", "required") => "Client and server".into(),
        ("optional", "required") => "Server-side, works in singleplayer".into(),
        (_, _) => "Client and server (optional)".into(),
    }
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let i = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[i..];
    let q = rest.chars().next()?;
    if q == '"' || q == '\'' { rest[1..].split(q).next().map(String::from) } else { rest.split([' ', '>']).next().map(String::from) }
}

/// Modrinth bodies mix Markdown with HTML; turn the common tags into Markdown and drop the rest.
pub fn html_to_md(s: &str) -> String {
    let mut out = String::new();
    let mut links: Vec<String> = vec![];
    let mut in_heading = false;
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        // Markdown headings must be one line: collapse the HTML heading's inner whitespace.
        let text = &rest[..i];
        if in_heading {
            let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if !t.is_empty() && !out.ends_with(' ') && !out.ends_with('#') && text.starts_with(char::is_whitespace) {
                out.push(' ');
            }
            out.push_str(&t);
            if !t.is_empty() && text.ends_with(char::is_whitespace) {
                out.push(' ');
            }
        } else {
            out.push_str(text);
        }
        let tail = &rest[i..];
        let looks_like_tag = tail[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!');
        let Some(j) = tail.find('>').filter(|_| looks_like_tag) else {
            out.push('<');
            rest = &tail[1..];
            continue;
        };
        let tag = &tail[1..j];
        let closing = tag.starts_with('/');
        let name: String =
            tag.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if matches!(name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
            in_heading = !closing;
        }
        match (name.as_str(), closing) {
            ("img", _) => {
                if let Some(src) = attr(tag, "src") {
                    out.push_str(&format!("![]({src})"));
                }
            }
            ("a", false) => {
                links.push(attr(tag, "href").unwrap_or_default());
                out.push('[');
            }
            ("a", true) => out.push_str(&format!("]({})", links.pop().unwrap_or_default())),
            ("br", _) => out.push_str("  \n"),
            ("p" | "div" | "ul" | "ol", _) | ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", true) => out.push_str("\n\n"),
            ("h1", false) => out.push_str("\n\n# "),
            ("h2", false) => out.push_str("\n\n## "),
            ("h3" | "h4" | "h5" | "h6", false) => out.push_str("\n\n### "),
            ("li", false) => out.push_str("\n- "),
            ("strong" | "b", _) => out.push_str("**"),
            ("em" | "i", _) => out.push('*'),
            ("hr", _) => out.push_str("\n\n---\n\n"),
            _ => {}
        }
        rest = &tail[j + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_conversion() {
        assert_eq!(html_to_md(r#"<p><img src="https://x/a.png" width=50></p>"#), "\n\n![](https://x/a.png)\n\n");
        // `<center>` has no Markdown equivalent; dropping it keeps "## <center>Title</center>" a heading
        assert_eq!(html_to_md("## <center> Title </center>\n"), "##  Title \n");
        assert_eq!(html_to_md(r#"<a href="https://y">site</a> & 1 < 2"#), "[site](https://y) & 1 < 2");
        assert_eq!(html_to_md("<h2>Hi</h2><b>bold</b>"), "\n\n## Hi\n\n**bold**");
        assert_eq!(html_to_md("<h2>\n  Tensura <b>Re</b>\n</h2>"), "\n\n## Tensura **Re**\n\n");
        assert_eq!(short(7_600), "7.6K");
    }
}

#[cfg(test)]
mod e2e {
    use super::*;

    /// Loads a real project and renders every tab headlessly (catches panics / layout bugs).
    /// cargo test project_page -- --ignored --nocapture
    #[test]
    #[ignore]
    fn project_page() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let target = Target::Server { dir: "/tmp".into(), mc: "1.19.2".into(), flavor: Flavor::Forge };
        let mut page = Page::open("tensura-reextended".into(), target);
        let frame = |page: &mut Page| {
            let mut out = None;
            let mut o = ctx.run_ui(egui::RawInput::default(), |ui| out = page.show(ui.ctx(), &mut true, false));
            o.textures_delta.clear();
            out
        };
        for _ in 0..200 {
            if page.data.lock().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let data = Arc::clone(&page.data);
        let guard = data.lock().unwrap();
        let d = guard.as_ref().unwrap().as_ref().unwrap();
        println!(
            "title={} status={} versions={} members={} gallery={}",
            d.p["title"],
            d.p["status"],
            d.versions.len(),
            d.members.len(),
            d.p["gallery"].as_array().map_or(0, |g| g.len())
        );
        println!("compatible(1.19.2 forge)={}", d.versions.iter().any(|v| page.compatible(v)));
        println!("env={}", environments(&d.p));
        println!("body md start:\n{}", d.body.chars().take(400).collect::<String>());
        drop(guard);
        for t in [Tab::Description, Tab::Gallery, Tab::Changelog, Tab::Versions] {
            page.tab = t;
            for _ in 0..3 {
                frame(&mut page);
            }
        }
        println!("rendered all tabs OK");
    }
}
