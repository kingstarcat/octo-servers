//! Self-update from the GitHub releases of the repo: check on startup, swap the exe in place.
use crate::{Log, download, get_json, push, s};
use std::path::PathBuf;

const REPO: &str = "kingstarcat/octo-servers";

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// Release asset for this platform.
fn asset_name() -> &'static str {
    if cfg!(windows) { "octo.exe" } else { "octo" }
}

fn newer(candidate: &str, current: &str) -> bool {
    let key = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    key(candidate) > key(current)
}

/// The latest release if it's newer than this build and has a download for this platform.
pub fn check() -> Result<Option<Release>, String> {
    let r = get_json(&format!("https://api.github.com/repos/{REPO}/releases/latest"))?;
    let version = r["tag_name"].as_str().ok_or("no release")?.trim_start_matches('v').to_string();
    if !newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let url =
        r["assets"].as_array().into_iter().flatten().find(|a| a["name"] == asset_name()).and_then(|a| a["browser_download_url"].as_str());
    Ok(url.map(|u| Release { version, url: u.to_string() }))
}

/// Download the new build and put it where the running exe is. Windows lets a running exe be
/// renamed (not overwritten), so the current one moves to `octo.old` and is deleted next start.
pub fn install(r: &Release, log: &Log) -> Result<PathBuf, String> {
    install_to(r, std::env::current_exe().map_err(s)?, log)
}

fn install_to(r: &Release, exe: PathBuf, log: &Log) -> Result<PathBuf, String> {
    let (new, old) = (exe.with_extension("new"), exe.with_extension("old"));
    push(log, format!("Downloading Octo Servers {}", r.version));
    download(&r.url, &new)?;
    #[cfg(unix)]
    std::fs::set_permissions(&new, std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(s)?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("Couldn't replace the program file: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("Couldn't replace the program file: {e}"));
    }
    push(log, "Update installed. Restart Octo Servers to use it.");
    Ok(exe)
}

/// Remove the previous build left behind by `install`.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("old"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare() {
        assert!(newer("0.2.0", "0.1.0"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.0.9", "0.1.0"));
    }

    /// Swap a stand-in "exe" for a downloaded file, like a real update. Needs the network.
    #[test]
    #[ignore]
    fn swaps_exe_in_place() {
        let d = std::env::temp_dir().join(format!("octo-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let exe = d.join("octo.exe");
        std::fs::write(&exe, "old build").unwrap();
        let r = Release { version: "9.9.9".into(), url: "https://raw.githubusercontent.com/rust-lang/rust/master/README.md".into() };
        install_to(&r, exe.clone(), &Log::default()).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("Rust"), "new build in place");
        assert_eq!(std::fs::read_to_string(d.join("octo.old")).unwrap(), "old build");
        assert!(!d.join("octo.new").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Replaces the *running* test binary. Only run on a throwaway copy:
    /// cp <test exe> /tmp/x.exe && wine /tmp/x.exe swaps_running_exe --ignored
    #[test]
    #[ignore]
    fn swaps_running_exe() {
        let exe = std::env::current_exe().unwrap();
        let r = Release { version: "9.9.9".into(), url: "https://raw.githubusercontent.com/rust-lang/rust/master/README.md".into() };
        install_to(&r, exe.clone(), &Log::default()).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("Rust"));
        assert!(exe.with_extension("old").exists());
    }
}
