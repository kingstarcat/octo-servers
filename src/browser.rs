//! CurseForge search through a hidden native browser, using its normal TLS, JS and cookies.
use crate::{Results, data_dir, flavors::Flavor, s, sources};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

pub struct Search {
    // Drop the view before its context.
    view: Option<WebView>,
    #[cfg(target_os = "linux")]
    offscreen: Option<gtk::OffscreenWindow>,
    context: Option<WebContext>,
    url: String,
    class: &'static str,
    out: Option<Results>,
    snapshot: Arc<Mutex<Option<String>>>,
    started: Instant,
    last_read: Instant,
    reading: bool,
    loaded: Arc<AtomicBool>,
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
    let mut v: Value = serde_json::from_str(raw).map_err(s)?;
    // WebView2 and WebKit serialize JavaScript return values differently.
    if let Some(inner) = v.as_str() {
        v = serde_json::from_str(inner).map_err(s)?;
    }
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

impl Search {
    pub fn new(query: &str, class: &'static str, mc: Option<&str>, flavor: Option<Flavor>, out: Results) -> Self {
        Self {
            view: None,
            #[cfg(target_os = "linux")]
            offscreen: None,
            context: None,
            url: sources::cf_search_url(query, class, mc, flavor),
            class,
            out: Some(out),
            snapshot: Default::default(),
            started: Instant::now(),
            last_read: Instant::now(),
            reading: false,
            loaded: Arc::new(AtomicBool::new(false)),
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
        self.context = Some(WebContext::new(Some(std::fs::canonicalize(profile).map_err(s)?)));
        let loaded = self.loaded.clone();
        let builder = WebViewBuilder::new_with_web_context(self.context.as_mut().unwrap())
            .with_visible(false)
            .with_on_page_load_handler(move |event, _| {
                loaded.store(matches!(event, wry::PageLoadEvent::Finished), Ordering::Relaxed);
            })
            .with_bounds(wry::Rect { position: wry::dpi::LogicalPosition::new(0, 0).into(), size: wry::dpi::LogicalSize::new(1, 1).into() })
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

    fn finish(&mut self, result: Result<Vec<sources::Hit>, String>) {
        if let Some(out) = self.out.take() {
            *out.lock().unwrap() = Some(result);
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
            self.finish(Err(format!("Couldn't search CurseForge: {e}. Paste a project link or try again.")));
            return true;
        }
        #[cfg(target_os = "linux")]
        while gtk::events_pending() {
            gtk::main_iteration_do(false);
        }
        let raw = self.snapshot.lock().unwrap().take();
        if let Some(raw) = raw {
            self.reading = false;
            match parse_snapshot(&raw, self.class) {
                Ok((hits, empty)) if !hits.is_empty() || empty => {
                    self.finish(Ok(hits));
                    return true;
                }
                _ => {}
            }
        }
        if self.started.elapsed() > Duration::from_secs(45) {
            self.finish(Err(
                "CurseForge didn't return search results. It may be asking for browser verification. Try again or paste a project link."
                    .into(),
            ));
            return true;
        }
        if self.loaded.load(Ordering::Relaxed) && !self.reading && self.last_read.elapsed() > Duration::from_millis(500) {
            let out = self.snapshot.clone();
            self.reading = true;
            self.last_read = Instant::now();
            if let Err(e) = self.view.as_ref().unwrap().evaluate_script_with_callback(EXTRACT, move |raw| *out.lock().unwrap() = Some(raw))
            {
                self.finish(Err(format!("Couldn't read CurseForge results: {e}. Try again.")));
                return true;
            }
        }
        false
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        if self.out.is_some() {
            self.finish(Err("Search cancelled. Search again.".into()));
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
