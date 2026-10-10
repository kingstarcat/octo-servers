//! CurseForge search and project lookup through a hidden native browser.
use crate::{Results, data_dir, flavors::Flavor, s, sources};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use wry::{WebContext, WebView, WebViewBuilder};

// ponytail: reads the site's result cards; update these selectors if CurseForge changes its markup.
const EXTRACT: &str = r#"(() => {
    const text = (card, selector) => card.querySelector(selector)?.textContent.trim() || '';
    const hits = Array.from(document.querySelectorAll('.project-card')).slice(0,40).map(card => {
        const a = card.querySelector('a.name');
        const count = text(card, '.detail-downloads').replaceAll(',', '');
        const multiplier = {K:1e3,M:1e6,B:1e9}[count.slice(-1).toUpperCase()] || 1;
        return {title:a?.textContent.trim() || '', slug:a?.href || '',
            description:text(card,'.description'), icon_url:card.querySelector('img')?.src || '',
            downloads:Math.round((parseFloat(count) || 0)*multiplier)};
    });
    return {hits, title:document.title, ready:document.readyState==='complete',
        empty:/no (results|projects|mods|modpacks) (found|match)/i.test(document.body?.innerText || '')};
})()"#;

// shortcut: reads the project's Details row; update if CurseForge changes its markup.
const EXTRACT_PROJECT: &str = r#"(() => {
    const text = document.querySelector('.project-id')?.textContent.trim() || '';
    const id = /^\d+$/.test(text) ? Number(text) : null;
    return {project_id:Number.isSafeInteger(id) && id > 0 ? id : null, url:location.href};
})()"#;

enum Output {
    Search { class: &'static str, results: Results },
    Project(mpsc::SyncSender<Result<u64, String>>),
}

struct ProjectRequest {
    url: String,
    out: mpsc::SyncSender<Result<u64, String>>,
    deadline: Instant,
}

static PROJECTS: Mutex<VecDeque<ProjectRequest>> = Mutex::new(VecDeque::new());
thread_local! {
    static CONTEXT: RefCell<Option<Rc<RefCell<WebContext>>>> = const { RefCell::new(None) };
}
// Automatic browser verification can take over a minute before the page loads.
const PAGE_TIMEOUT: Duration = Duration::from_secs(150);
const PROJECT_TIMEOUT: Duration = Duration::from_secs(170);

/// Called by install workers; the app loads the page on its UI thread.
pub fn project_id(url: &str) -> Result<u64, String> {
    let uri = url.parse::<wry::http::Uri>().map_err(s)?;
    if !project_link(url, uri.path().split('/').nth(2).unwrap_or("")) {
        return Err("Couldn't read the CurseForge link. Paste the project's CurseForge page link and try again.".into());
    }
    let (out, result) = mpsc::sync_channel(1);
    PROJECTS.lock().unwrap().push_back(ProjectRequest { url: url.into(), out, deadline: Instant::now() + PROJECT_TIMEOUT });
    result.recv_timeout(PROJECT_TIMEOUT).map_err(|_| "Couldn't load the CurseForge project page. Try adding the link again.")?
}

pub fn poll_projects(active: &mut Vec<Search>, frame: &eframe::Frame) {
    for request in PROJECTS.lock().unwrap().drain(..) {
        if request.deadline > Instant::now() {
            active.push(Search::project(request));
        }
    }
    active.retain_mut(|lookup| !lookup.poll(frame));
}

pub struct Search {
    // Drop the view before its context.
    view: Option<WebView>,
    #[cfg(target_os = "linux")]
    offscreen: Option<gtk::OffscreenWindow>,
    context: Option<Rc<RefCell<WebContext>>>,
    url: String,
    out: Option<Output>,
    snapshot: Arc<Mutex<Option<String>>>,
    started: Instant,
    last_read: Instant,
    reading: bool,
}

fn project_link(url: &str, class: &str) -> bool {
    let Ok(uri) = url.parse::<wry::http::Uri>() else { return false };
    if uri.scheme_str() != Some("https") || !matches!(uri.host(), Some("curseforge.com" | "www.curseforge.com")) {
        return false;
    }
    let parts: Vec<_> = uri.path().split('/').filter(|x| !x.is_empty()).collect();
    parts.len() == 3 && parts[0] == "minecraft" && parts[1] == class
}

fn parse_snapshot(raw: &str, class: &str) -> Result<(Vec<sources::Hit>, bool), String> {
    let v = snapshot_json(raw)?;
    let rows = v["hits"].as_array().ok_or("Couldn't read CurseForge search results. Try searching again.")?;
    let mut seen = std::collections::HashSet::new();
    let hits = rows
        .iter()
        .filter_map(|h| {
            let link = h["slug"].as_str()?;
            let title = h["title"].as_str()?;
            if title.is_empty() || !project_link(link, class) || !seen.insert(link.to_string()) {
                return None;
            }
            Some(sources::Hit {
                title: title.into(),
                slug: link.into(),
                description: h["description"].as_str().unwrap_or("").into(),
                icon_url: h["icon_url"].as_str().unwrap_or("").into(),
                downloads: h["downloads"].as_u64().unwrap_or(0),
            })
        })
        .collect();
    Ok((hits, v["ready"] == true && v["empty"] == true))
}

fn snapshot_json(raw: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(raw).map_err(s)?;
    // WebView2 and WebKit serialize JavaScript return values differently.
    if let Some(inner) = v.as_str() { serde_json::from_str(inner).map_err(s) } else { Ok(v) }
}

fn parse_project_snapshot(raw: &str, expected: &str) -> Result<Option<u64>, String> {
    let v = snapshot_json(raw)?;
    let Some(id) = v["project_id"].as_u64().filter(|id| *id > 0) else { return Ok(None) };
    let actual = v["url"].as_str().unwrap_or("");
    let expected = expected.parse::<wry::http::Uri>().map_err(s)?;
    let class = expected.path().split('/').nth(2).unwrap_or("");
    if !project_link(actual, class)
        || actual.parse::<wry::http::Uri>().map_err(s)?.path().trim_end_matches('/') != expected.path().trim_end_matches('/')
    {
        return Err("CurseForge opened a different project page. Check the link and try again.".into());
    }
    Ok(Some(id))
}

impl Search {
    pub fn new(query: &str, class: &'static str, mc: Option<&str>, flavor: Option<Flavor>, out: Results) -> Self {
        Self::page(sources::cf_search_url(query, class, mc, flavor), Output::Search { class, results: out })
    }

    fn project(request: ProjectRequest) -> Self {
        Self::page(request.url, Output::Project(request.out))
    }

    fn page(url: String, out: Output) -> Self {
        Self {
            view: None,
            #[cfg(target_os = "linux")]
            offscreen: None,
            context: None,
            url,
            out: Some(out),
            snapshot: Default::default(),
            started: Instant::now(),
            last_read: Instant::now(),
            reading: false,
        }
    }

    fn create(&mut self, _frame: &eframe::Frame) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            gtk::gdk::set_allowed_backends("x11");
            gtk::init().map_err(s)?;
        }
        let profile = data_dir().join("browser-profile");
        std::fs::create_dir_all(&profile).map_err(s)?;
        let profile = std::fs::canonicalize(profile).map_err(s)?;
        // Keep live cookies and connections shared between search and install pages.
        self.context = Some(
            CONTEXT
                .with(|context| context.borrow_mut().get_or_insert_with(|| Rc::new(RefCell::new(WebContext::new(Some(profile))))).clone()),
        );
        let mut context = self.context.as_ref().unwrap().borrow_mut();
        let builder = WebViewBuilder::new_with_web_context(&mut context)
            .with_visible(cfg!(target_os = "linux"))
            .with_bounds(wry::Rect {
                position: wry::dpi::LogicalPosition::new(0, 0).into(),
                size: wry::dpi::LogicalSize::new(1180, 760).into(),
            })
            .with_navigation_handler(|url| url.parse::<wry::http::Uri>().is_ok_and(|u| matches!(u.scheme_str(), Some("https" | "http"))))
            .with_download_started_handler(|_, _| false);
        #[cfg(not(target_os = "linux"))]
        let view = builder.build_as_child(_frame).map_err(s)?;
        #[cfg(target_os = "linux")]
        let view = {
            use gtk::prelude::*;
            use wry::WebViewBuilderExtUnix;
            // OffscreenWindow renders into memory and has no surface on the user's desktop.
            // A hidden child webview still creates an X11/GTK window that can flash or overlay Octo.
            let offscreen = gtk::OffscreenWindow::new();
            offscreen.set_default_size(1180, 760);
            let view = builder.build_gtk(&offscreen).map_err(s)?;
            self.offscreen = Some(offscreen);
            view
        };
        #[cfg(target_os = "linux")]
        {
            use webkit2gtk::{SettingsExt, WebViewExt};
            use wry::WebViewExtUnix;
            if let Some(settings) = view.webview().settings() {
                // GTK offscreen windows cannot supply a GPU context.
                settings.set_hardware_acceleration_policy(webkit2gtk::HardwareAccelerationPolicy::Never);
            }
        }
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::*;
            self.offscreen.as_ref().unwrap().show_all();
        }
        view.load_url(&self.url).map_err(s)?;
        self.view = Some(view);
        Ok(())
    }

    fn fail(&mut self, error: String) {
        if let Some(out) = self.out.take() {
            match out {
                Output::Search { results, .. } => *results.lock().unwrap() = Some(Err(error)),
                Output::Project(out) => {
                    let _ = out.send(Err(error));
                }
            }
        }
    }

    /// Called on the UI thread. Networking and JavaScript run in the browser process.
    pub fn poll(&mut self, frame: &eframe::Frame) -> bool {
        if self.out.is_none() {
            return true;
        }
        if self.view.is_none()
            && let Err(e) = self.create(frame)
        {
            self.fail(format!("Couldn't load CurseForge in the background browser: {e}. Try again."));
            return true;
        }
        #[cfg(target_os = "linux")]
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        let raw = self.snapshot.lock().unwrap().take();
        if let Some(raw) = raw {
            self.reading = false;
            match self.out.as_ref().unwrap() {
                Output::Search { class, results } => {
                    if let Ok((hits, empty)) = parse_snapshot(&raw, class)
                        && (!hits.is_empty() || empty)
                    {
                        *results.lock().unwrap() = Some(Ok(hits));
                        self.out.take();
                        return true;
                    }
                }
                Output::Project(out) => match parse_project_snapshot(&raw, &self.url) {
                    Ok(Some(id)) => {
                        let _ = out.send(Ok(id));
                        self.out.take();
                        return true;
                    }
                    Err(e) => {
                        self.fail(e);
                        return true;
                    }
                    Ok(None) => {}
                },
            }
        }
        if self.started.elapsed() > PAGE_TIMEOUT {
            self.fail(
                "CurseForge didn't finish loading in the background browser. It may be asking for browser verification. Try again later."
                    .into(),
            );
            return true;
        }
        // Read as soon as the DOM contains the ID; ads can delay the page's load event.
        if !self.reading && self.last_read.elapsed() > Duration::from_millis(500) {
            let out = self.snapshot.clone();
            self.reading = true;
            self.last_read = Instant::now();
            let script = match self.out.as_ref().unwrap() {
                Output::Search { .. } => EXTRACT,
                Output::Project(_) => EXTRACT_PROJECT,
            };
            if let Err(e) = self.view.as_ref().unwrap().evaluate_script_with_callback(script, move |raw| *out.lock().unwrap() = Some(raw)) {
                self.fail(format!("Couldn't read the CurseForge page: {e}. Try again."));
                return true;
            }
        }
        false
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        if self.out.is_some() {
            self.fail("CurseForge page lookup cancelled. Try again.".into());
        }
        #[cfg(target_os = "linux")]
        if let Some(offscreen) = self.offscreen.take() {
            use gtk::prelude::*;
            self.view.take();
            offscreen.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_project_id_only_from_the_requested_page() {
        let expected = "https://www.curseforge.com/minecraft/mc-mods/ferritecore";
        let sample = serde_json::json!({"project_id":429235, "url":expected});
        for raw in [sample.to_string(), serde_json::to_string(&sample.to_string()).unwrap()] {
            assert_eq!(parse_project_snapshot(&raw, expected).unwrap(), Some(429235));
        }
        for id in [Value::Null, serde_json::json!(0), serde_json::json!(-1), serde_json::json!("429235")] {
            assert_eq!(parse_project_snapshot(&serde_json::json!({"project_id":id, "url":expected}).to_string(), expected).unwrap(), None);
        }
        for url in [
            "https://evil.test/minecraft/mc-mods/ferritecore",
            "https://www.curseforge.com/minecraft/mc-mods/simple-voice-chat",
            "https://www.curseforge.com/minecraft/modpacks/ferritecore",
        ] {
            assert!(parse_project_snapshot(&serde_json::json!({"project_id":429235, "url":url}).to_string(), expected).is_err());
        }
        assert!(project_id("https://evil.test/minecraft/mc-mods/ferritecore").is_err());
    }

    #[test]
    fn closing_a_project_lookup_unblocks_the_worker() {
        let (out, result) = mpsc::sync_channel(1);
        let lookup = Search::project(ProjectRequest {
            url: "https://www.curseforge.com/minecraft/mc-mods/ferritecore".into(),
            out,
            deadline: Instant::now() + Duration::from_secs(60),
        });
        drop(lookup);
        assert!(result.try_recv().unwrap().unwrap_err().contains("cancelled"));
    }

    #[test]
    fn reads_results_and_rejects_other_hosts() {
        let sample = serde_json::json!({"hits":[
            {"title":"Just Enough Items", "slug":"https://www.curseforge.com/minecraft/mc-mods/jei", "description":"Recipes", "downloads":630900000},
            {"title":"Duplicate", "slug":"https://www.curseforge.com/minecraft/mc-mods/jei"},
            {"title":"Wrong host", "slug":"https://evil.test/minecraft/mc-mods/jei"},
            {"title":"Wrong class", "slug":"https://www.curseforge.com/minecraft/modpacks/pack"}
        ], "ready":true, "empty":false});
        for raw in [sample.to_string(), serde_json::to_string(&sample.to_string()).unwrap()] {
            let (hits, empty) = parse_snapshot(&raw, "mc-mods").unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].title, "Just Enough Items");
            assert_eq!(hits[0].downloads, 630900000);
            assert!(!empty);
        }
        assert!(parse_snapshot("null", "mc-mods").is_err());
        assert!(parse_snapshot(r#"{"hits":[],"ready":true,"empty":true}"#, "mc-mods").unwrap().1);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod live {
    use super::*;
    use eframe::egui;

    /// Run under a separate X server with OCTO_DATA_DIR pointing at a scratch directory.
    #[test]
    #[ignore]
    fn background_curseforge_project_lookup() {
        struct Probe {
            active: Vec<Search>,
            context: Option<Rc<RefCell<WebContext>>>,
            result: Arc<Mutex<Option<Result<(), String>>>>,
            started: Instant,
        }
        impl eframe::App for Probe {
            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                ui.ctx().request_repaint_after(Duration::from_millis(50));
                poll_projects(&mut self.active, frame);
                for lookup in &self.active {
                    if let Some(context) = &lookup.context {
                        if let Some(previous) = &self.context {
                            assert!(Rc::ptr_eq(previous, context), "Lookups must share the browser session");
                        } else {
                            self.context = Some(context.clone());
                        }
                    }
                    if let Some(offscreen) = &lookup.offscreen {
                        use gtk::prelude::*;
                        assert_eq!(offscreen.window().unwrap().window_type(), gtk::gdk::WindowType::Offscreen);
                    }
                }
                if self.started.elapsed() > Duration::from_secs(1200) {
                    *self.result.lock().unwrap() = Some(Err("Project lookup test timed out".into()));
                }
                if self.result.lock().unwrap().is_some() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        let result = Arc::new(Mutex::new(None));
        let worker_result = result.clone();
        let worker = std::thread::spawn(move || {
            let check = || -> Result<(), String> {
                for (class, slug, expected) in [
                    ("modpacks", "fabulously-optimized", 396246),
                    ("mc-mods", "ferritecore", 429235),
                    ("mc-mods", "simple-voice-chat", 416089),
                ] {
                    let url = format!("https://www.curseforge.com/minecraft/{class}/{slug}");
                    let started = Instant::now();
                    let browser_id = project_id(&url)?;
                    println!("{slug}: browser lookup took {:.1}s", started.elapsed().as_secs_f32());
                    if browser_id != expected {
                        return Err(format!("{slug}: expected {expected}, browser got {browser_id}"));
                    }
                    // Exercise the real cfwidget miss as well as the browser queue used by installers.
                    let started = Instant::now();
                    let id = sources::cf_project_id(&sources::parse_cf(&url).unwrap())?;
                    if id != expected {
                        return Err(format!("{slug}: expected {expected}, got {id}"));
                    }
                    println!("{slug}: resolved project {id}, fallback took {:.1}s", started.elapsed().as_secs_f32());
                }
                Ok(())
            };
            *worker_result.lock().unwrap() = Some(check());
        });
        let observed = result.clone();
        let options = eframe::NativeOptions {
            event_loop_builder: Some(Box::new(|builder| {
                use winit::platform::x11::EventLoopBuilderExtX11;
                builder.with_x11().with_any_thread(true);
            })),
            viewport: egui::ViewportBuilder::default().with_visible(false),
            ..Default::default()
        };
        eframe::run_native(
            "Octo project lookup check",
            options,
            Box::new(|_| Ok(Box::new(Probe { active: vec![], context: None, result, started: Instant::now() }))),
        )
        .unwrap();
        worker.join().unwrap();
        observed.lock().unwrap().take().unwrap().unwrap();
    }

    /// Tests background search and verifies that Linux uses an offscreen surface.
    /// cargo test background_curseforge_search -- --ignored --nocapture --test-threads=1
    #[test]
    #[ignore]
    fn background_curseforge_search() {
        struct Probe {
            search: Option<Search>,
            results: Results,
            step: usize,
            failure: Arc<Mutex<Option<String>>>,
        }
        impl eframe::App for Probe {
            fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
                ui.ctx().request_repaint_after(Duration::from_millis(50));
                ui.label("Checking background CurseForge search");
                if self.step >= 3 || self.failure.lock().unwrap().is_some() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    return;
                }
                if self.search.is_none() {
                    self.results = Default::default();
                    self.search = Some(match self.step {
                        0 => Search::new("jei", "mc-mods", Some("1.20.1"), Some(Flavor::Forge), self.results.clone()),
                        1 => Search::new("all the mods", "modpacks", None, None, self.results.clone()),
                        _ => Search::new("octo-no-matches-829347829347", "mc-mods", None, None, self.results.clone()),
                    });
                }
                let done = self.search.as_mut().unwrap().poll(frame);
                if let Some(offscreen) = &self.search.as_ref().unwrap().offscreen {
                    use gtk::prelude::*;
                    assert_eq!(offscreen.window().unwrap().window_type(), gtk::gdk::WindowType::Offscreen);
                }
                if done {
                    let result = self.results.lock().unwrap().take().unwrap();
                    match result {
                        Ok(hits) if self.step == 2 && hits.is_empty() => {
                            println!("Search 2: no results, as expected");
                        }
                        Ok(hits) if self.step < 2 && !hits.is_empty() => {
                            println!("Search {}: {} results, first = {} ({})", self.step, hits.len(), hits[0].title, hits[0].slug);
                        }
                        other => {
                            *self.failure.lock().unwrap() = Some(match other {
                                Ok(_) => "Expected live search results".into(),
                                Err(e) => e,
                            });
                        }
                    }
                    self.search = None;
                    self.step += 1;
                    if self.step == 3 || self.failure.lock().unwrap().is_some() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
        }
        let scratch = std::env::temp_dir().join(format!("octo-background-browser-{}", std::process::id()));
        unsafe { std::env::set_var("OCTO_DATA_DIR", &scratch) };
        let failure = Arc::new(Mutex::new(None));
        let observed = failure.clone();
        let options = eframe::NativeOptions {
            event_loop_builder: Some(Box::new(|builder| {
                use winit::platform::x11::EventLoopBuilderExtX11;
                builder.with_x11().with_any_thread(true);
            })),
            viewport: egui::ViewportBuilder::default().with_inner_size([400.0, 120.0]),
            ..Default::default()
        };
        eframe::run_native(
            "Octo background browser check",
            options,
            Box::new(|_| Ok(Box::new(Probe { search: None, results: Default::default(), step: 0, failure }))),
        )
        .unwrap();
        let failure = observed.lock().unwrap().clone();
        assert!(failure.is_none(), "{failure:?}");
    }
}
