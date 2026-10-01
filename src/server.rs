use crate::flavors::{self, Flavor};
use crate::{Log, cmd, data_dir, java, push, s, spawn_logged};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub flavor: Flavor,
    pub mc_version: String,
    pub java_major: u32,
    pub ram_mb: u32,
    /// Ready-to-run packs: the pack's own java arguments instead of the flavor's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<Vec<String>>,
}

pub struct Server {
    pub name: String,
    pub dir: PathBuf,
    pub cfg: Config,
    pub console: Log,
    /// Set by Restart: start again once the server has exited.
    pub restart: bool,
    /// Why the server stopped on its own (not via Stop / "stop"), for the dashboard.
    pub crash: Option<String>,
    user_stopped: bool,
    child: Option<Child>,
    started: Option<Instant>,
}

pub fn servers_dir() -> PathBuf {
    data_dir().join("servers")
}

pub fn validate_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Enter a name".into());
    }
    if n.len() > 40 || !n.chars().all(|c| c.is_ascii_alphanumeric() || " -_".contains(c)) {
        return Err("Use letters, numbers, spaces, - and _ (max 40)".into());
    }
    if servers_dir().join(n).exists() {
        return Err("A server with that name already exists".into());
    }
    Ok(())
}

pub fn load_all() -> Vec<Server> {
    let entries = std::fs::read_dir(servers_dir()).into_iter().flatten().flatten();
    entries.filter_map(|e| load(e.path())).collect()
}

/// A server folder with an octo.json; its name is the folder name.
pub fn load(dir: PathBuf) -> Option<Server> {
    let cfg = serde_json::from_str(&std::fs::read_to_string(dir.join("octo.json")).ok()?).ok()?;
    let name = dir.file_name()?.to_string_lossy().into();
    Some(Server { name, dir, cfg, console: Log::default(), restart: false, crash: None, user_stopped: false, child: None, started: None })
}

pub fn create(name: &str, cfg: Config, log: &Log) -> Result<(), String> {
    let ram = cfg.ram_mb;
    create_with(name, ram, log, |_, _| Ok((cfg.flavor, cfg.mc_version, None)))
}

/// `fill` populates the new server folder (e.g. from a modpack) and says which
/// flavor / MC version / exact loader version it needs; then the loader is installed on top.
pub fn create_with(
    name: &str,
    ram_mb: u32,
    log: &Log,
    fill: impl FnOnce(&Path, &Log) -> Result<(Flavor, String, Option<String>), String>,
) -> Result<(), String> {
    validate_name(name)?;
    let dir = servers_dir().join(name);
    std::fs::create_dir_all(&dir).map_err(s)?;
    let res = (|| {
        let (flavor, mc_version, loader) = fill(&dir, log)?;
        push(log, format!("{flavor:?} {mc_version} {}", loader.as_deref().unwrap_or("")));
        push(log, format!("Checking which Java Minecraft {mc_version} needs..."));
        let launch = crate::packs::ready_launch(&dir);
        let mut java_major = flavors::java_major(&mc_version)?;
        if launch.as_ref().is_some_and(|a| crate::packs::needs_modern_java(&dir, a)) && java_major < 17 {
            java_major = 21;
        }
        let cfg = Config { flavor, java_major, mc_version, ram_mb, launch };
        let java = java::ensure(cfg.java_major, log)?;
        push(log, format!("Using {}", java.display()));
        match &cfg.launch {
            Some(args) => push(log, format!("Pack is ready to run, using its own launch command: java {}", args.join(" "))),
            None => flavors::install(cfg.flavor, &cfg.mc_version, loader.as_deref(), &dir, &java, log)?,
        }
        // Only reached when the user ticked the EULA box in the dialog.
        std::fs::write(dir.join("eula.txt"), "eula=true\n").map_err(s)?;
        // Packs often ship with the whitelist on (GTNH does), which locks friends out. New servers
        // (no server.properties yet) already default to off.
        let props = dir.join("server.properties");
        if let Ok(text) = std::fs::read_to_string(&props) {
            let text = crate::dashboard::props_set(&text, &[("white-list", "false"), ("enforce-whitelist", "false")]);
            std::fs::write(&props, text).map_err(s)?;
        }
        save(&dir, &cfg)
    })();
    if res.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    res
}

fn save(dir: &Path, cfg: &Config) -> Result<(), String> {
    std::fs::write(dir.join("octo.json"), serde_json::to_string_pretty(cfg).map_err(s)?).map_err(s)
}

impl Server {
    pub fn save(&self) -> Result<(), String> {
        save(&self.dir, &self.cfg)
    }

    pub fn port(&self) -> u16 {
        std::fs::read_to_string(self.dir.join("server.properties"))
            .ok()
            .and_then(|p| p.lines().find_map(|l| l.strip_prefix("server-port=")?.trim().parse().ok()))
            .unwrap_or(25565)
    }

    pub fn running(&mut self) -> bool {
        match &mut self.child {
            Some(c) => match c.try_wait() {
                Ok(None) => true,
                Ok(Some(st)) => {
                    push(&self.console, format!("[octo] server exited ({st})"));
                    self.child = None;
                    let lines = self.console.lock().unwrap().clone();
                    // a /stop from in-game also exits cleanly and logs "Stopping the server"
                    let clean = st.success()
                        && (self.user_stopped
                            || lines.iter().rev().take(200).any(|l| l.contains("Stopping the server") || l.contains("Stopping server")));
                    if !clean && !self.restart {
                        self.crash = Some(diagnose(&lines, self.cfg.java_major, self.port()));
                    }
                    false
                }
                Err(_) => false,
            },
            None => false,
        }
    }

    pub fn start(&mut self, java: &Path) -> Result<(), String> {
        let args = match &self.cfg.launch {
            Some(a) => a.clone(),
            None => flavors::launch_args(self.cfg.flavor, &self.dir)?,
        };
        let mut c = cmd(java);
        c.arg(format!("-Xmx{}M", self.cfg.ram_mb)).args(&args).current_dir(&self.dir);
        push(&self.console, format!("[octo] {} -Xmx{}M {}", java.display(), self.cfg.ram_mb, args.join(" ")));
        self.child = Some(spawn_logged(c, &self.console)?);
        self.started = Some(Instant::now());
        self.crash = None;
        self.user_stopped = false;
        Ok(())
    }

    pub fn uptime(&self) -> Option<Duration> {
        self.child.as_ref().and(self.started).map(|t| t.elapsed())
    }

    pub fn send(&mut self, line: &str) {
        if let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) {
            push(&self.console, format!("> {line}"));
            if line.trim() == "stop" {
                self.user_stopped = true;
            }
            let _ = writeln!(stdin, "{line}");
        }
    }

    /// Graceful: asks the server to save and exit.
    pub fn stop(&mut self) {
        self.send("stop");
    }

    pub fn wait_or_kill(&mut self, timeout: Duration) {
        let Some(c) = &mut self.child else { return };
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            if let Ok(Some(_)) = c.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = c.kill();
    }
}

/// Plain-language reason for a crash, from the console output.
pub fn diagnose(lines: &[String], java: u32, port: u16) -> String {
    let has = |pats: &[&str]| lines.iter().any(|l| pats.iter().any(|p| l.contains(p)));
    if has(&["OutOfMemoryError"]) {
        "It ran out of memory. Give it more RAM on the dashboard, or remove some mods.".into()
    } else if has(&["FAILED TO BIND TO PORT", "Address already in use"]) {
        format!("Port {port} is already in use. Another server is probably still running; close it or change the port in Settings.")
    } else if has(&["UnsupportedClassVersionError", "compiled by a more recent version of the Java Runtime"]) {
        format!("Part of the server needs a newer Java than Java {java}. The pack or mod may not support this Minecraft version.")
    } else if has(&["Unrecognized option", "Could not create the Java Virtual Machine", "Unrecognized VM option"]) {
        format!("Java {java} refused the start options. The pack probably expects a different Java version.")
    } else if has(&["invalid dist DEDICATED_SERVER", "net/minecraft/client/", "Environment type SERVER", "net.minecraft.client.Minecraft"])
    {
        "A client-only mod is installed (minimaps, shaders and HUD mods are common ones). Remove it from the mods folder and start again."
            .into()
    } else if has(&["Missing or unsupported mandatory dependencies", "Incompatible mods found", "which is missing", "Missing mods"]) {
        "A mod is missing something it depends on. The console shows which mod and what it needs.".into()
    } else if has(&["agree to the EULA"]) {
        "The Minecraft EULA hasn't been accepted for this server. Set eula=true in eula.txt in the server folder.".into()
    } else if has(&["Unable to access jarfile", "Error: Unable to access", "Could not find or load main class"]) {
        "Some server files are missing. Try creating the server again.".into()
    } else {
        "It stopped unexpectedly. The console and the crash-reports folder show what went wrong.".into()
    }
}

/// Folders holding the world: level-name (default "world") plus Paper's split dimensions.
fn world_dirs(dir: &Path) -> Vec<PathBuf> {
    let props = std::fs::read_to_string(dir.join("server.properties")).unwrap_or_default();
    let level = crate::dashboard::props_get(&props, "level-name").filter(|l| !l.is_empty()).unwrap_or("world".into());
    [level.clone(), format!("{level}_nether"), format!("{level}_the_end")].into_iter().map(|l| dir.join(l)).filter(|p| p.is_dir()).collect()
}

pub fn has_world(dir: &Path) -> bool {
    !world_dirs(dir).is_empty()
}

/// "2026-10-01_1412" in UTC, from the system clock.
fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}_{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

const KEEP_AUTO_BACKUPS: usize = 5;

/// Zip the world folders into `<server>/backups/`. Automatic ones are pruned to the newest few.
pub fn backup(dir: &Path, auto: bool, log: &Log) -> Result<PathBuf, String> {
    let worlds = world_dirs(dir);
    if worlds.is_empty() {
        return Err("There is no world to back up yet. Start the server once first.".into());
    }
    let out_dir = dir.join("backups");
    std::fs::create_dir_all(&out_dir).map_err(s)?;
    let base = format!("{}{}", if auto { "auto-" } else { "" }, stamp());
    let mut name = format!("{base}.zip");
    for i in 2.. {
        if !out_dir.join(&name).exists() {
            break;
        }
        name = format!("{base}-{i}.zip"); // two backups in the same second
    }
    let out = out_dir.join(&name);
    let tmp = out.with_extension("part");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&tmp).map_err(s)?);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true);
    let mut n = 0;
    fn add(
        z: &mut zip::ZipWriter<std::fs::File>,
        base: &Path,
        p: &Path,
        opts: zip::write::SimpleFileOptions,
        n: &mut usize,
    ) -> Result<(), String> {
        for e in std::fs::read_dir(p).map_err(s)?.flatten() {
            let path = e.path();
            let rel = path.strip_prefix(base).map_err(s)?.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                add(z, base, &path, opts, n)?;
            } else if path.file_name().is_some_and(|f| f != "session.lock") {
                z.start_file(rel, opts).map_err(s)?;
                std::io::copy(&mut std::fs::File::open(&path).map_err(s)?, z).map_err(s)?;
                *n += 1;
            }
        }
        Ok(())
    }
    for w in &worlds {
        push(log, format!("Backing up {}", w.file_name().unwrap_or_default().to_string_lossy()));
        add(&mut z, dir, w, opts, &mut n)?;
    }
    z.finish().map_err(s)?;
    std::fs::rename(&tmp, &out).map_err(s)?;
    push(log, format!("Saved backups/{name} ({n} files)"));
    if auto {
        let mut autos: Vec<PathBuf> = std::fs::read_dir(&out_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|f| f.to_string_lossy().starts_with("auto-") && f.to_string_lossy().ends_with(".zip")))
            .collect();
        autos.sort();
        for old in autos.iter().rev().skip(KEEP_AUTO_BACKUPS) {
            let _ = std::fs::remove_file(old);
        }
    }
    Ok(out)
}

/// Total physical memory in MB, if the OS tells us.
pub fn system_ram_mb() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: m is a properly sized, zeroed MEMORYSTATUSEX with dwLength set, as the API requires.
        return (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then(|| m.ullTotalPhys / (1024 * 1024));
    }
    #[cfg(not(windows))]
    {
        let info = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = info.lines().find_map(|l| l.strip_prefix("MemTotal:"))?.trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb / 1024)
    }
}

/// Warning text when `ram_mb` leaves too little for the rest of the PC.
pub fn ram_warning(ram_mb: u32) -> Option<String> {
    let total = system_ram_mb()?;
    (ram_mb as u64 + 2048 > total)
        .then(|| format!("This PC has {:.0} GB of RAM. Leave at least 2 GB for Windows and other programs.", total as f64 / 1024.0))
}

#[cfg(test)]
mod tests {
    #[test]
    fn whitelist_turned_off() {
        let text = "#Minecraft server properties\nmotd=GTNH\nwhite-list=true\nmax-players=10\n";
        let out = crate::dashboard::props_set(text, &[("white-list", "false"), ("enforce-whitelist", "false")]);
        assert!(out.contains("white-list=false") && !out.contains("white-list=true"));
        assert!(out.contains("enforce-whitelist=false"));
        assert!(out.starts_with("#Minecraft server properties\nmotd=GTNH\n") && out.contains("max-players=10"));
    }
    use super::*;

    #[test]
    fn crash_messages() {
        let l = |s: &str| vec![s.to_string()];
        assert!(diagnose(&l("java.lang.OutOfMemoryError: Java heap space"), 21, 25565).contains("ran out of memory"));
        assert!(diagnose(&l("**** FAILED TO BIND TO PORT!"), 21, 25570).contains("Port 25570"));
        assert!(
            diagnose(&l("Attempted to load class net/minecraft/client/gui/Screen for invalid dist DEDICATED_SERVER"), 17, 1)
                .contains("client-only mod")
        );
        assert!(
            diagnose(&l("has been compiled by a more recent version of the Java Runtime (class file version 65.0)"), 17, 1)
                .contains("newer Java than Java 17")
        );
        assert!(diagnose(&l("random"), 21, 1).contains("stopped unexpectedly"));
    }

    #[cfg(unix)]
    #[test]
    fn timestamp_matches_date() {
        let want = String::from_utf8(std::process::Command::new("date").args(["-u", "+%Y-%m-%d_%H%M"]).output().unwrap().stdout).unwrap();
        assert!(stamp().starts_with(want.trim()), "{} vs {want}", stamp());
    }

    #[test]
    fn backups_zip_world_and_prune() {
        let d = std::env::temp_dir().join(format!("octo-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("world/region")).unwrap();
        std::fs::create_dir_all(d.join("world_nether")).unwrap();
        std::fs::write(d.join("world/level.dat"), "lvl").unwrap();
        std::fs::write(d.join("world/region/r.0.0.mca"), vec![7u8; 10_000]).unwrap();
        std::fs::write(d.join("world/session.lock"), "x").unwrap();
        std::fs::write(d.join("world_nether/level.dat"), "n").unwrap();
        let log = Log::default();
        let first = backup(&d, false, &log).unwrap();
        let mut z = zip::ZipArchive::new(std::fs::File::open(&first).unwrap()).unwrap();
        let mut names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, ["world/level.dat", "world/region/r.0.0.mca", "world_nether/level.dat"]);
        for _ in 0..7 {
            backup(&d, true, &log).unwrap();
        }
        let files: Vec<String> =
            std::fs::read_dir(d.join("backups")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into()).collect();
        assert_eq!(files.iter().filter(|f| f.starts_with("auto-")).count(), KEEP_AUTO_BACKUPS);
        assert_eq!(files.iter().filter(|f| !f.starts_with("auto-")).count(), 1, "manual backups are never pruned");
        assert!(backup(&d.join("nope"), false, &log).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn ram_detected() {
        let mb = system_ram_mb().unwrap();
        assert!(mb > 1024, "{mb}");
        assert!(ram_warning(mb as u32).is_some() && ram_warning(1024).is_none());
    }

    /// A fake "java" that crashes, versus one stopped with the stop command.
    #[cfg(unix)]
    #[test]
    fn crash_detection() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("octo-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let java = d.join("java");
        let script = "#!/bin/sh\nif [ -f crash ]; then echo 'java.lang.OutOfMemoryError: Java heap space'; exit 1; fi\nread line; echo 'Stopping the server'\n";
        std::fs::write(&java, script).unwrap();
        std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = Config { flavor: Flavor::Vanilla, mc_version: "1.21.1".into(), java_major: 21, ram_mb: 1024, launch: Some(vec![]) };
        let mut srv = Server {
            name: "t".into(),
            dir: d.clone(),
            cfg,
            console: Log::default(),
            restart: false,
            crash: None,
            user_stopped: false,
            child: None,
            started: None,
        };
        let wait = |srv: &mut Server| {
            for _ in 0..100 {
                if !srv.running() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("fake server didn't exit");
        };
        std::fs::write(d.join("crash"), "").unwrap();
        srv.start(&java).unwrap();
        wait(&mut srv);
        assert!(srv.crash.as_deref().unwrap().contains("ran out of memory"));
        std::fs::remove_file(d.join("crash")).unwrap();
        srv.start(&java).unwrap();
        assert!(srv.crash.is_none(), "start clears the old crash");
        srv.stop();
        wait(&mut srv);
        assert!(srv.crash.is_none(), "{:?}", srv.crash);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
