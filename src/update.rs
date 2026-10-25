//! Self-update from the GitHub releases of the repo: check on startup, swap the exe in place.
use crate::{Log, download, get_json, push, s};
use std::path::PathBuf;

const REPO: &str = "kingstarcat/octo-servers";

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// GitHub's sha256 of the asset (older releases may not have one)
    pub sha256: Option<String>,
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
    let Some(a) = r["assets"].as_array().into_iter().flatten().find(|a| a["name"] == asset_name()) else { return Ok(None) };
    let sha256 = a["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).map(String::from);
    Ok(a["browser_download_url"].as_str().map(|u| Release { version, url: u.to_string(), sha256 }))
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
    if let Some(h) = &r.sha256 {
        crate::verify(&new, h)?;
    }
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

/// A Discord webhook built into the previous build (`octo.old`, there until `cleanup`).
pub fn old_webhook() -> Option<String> {
    webhook_in(&std::fs::read(std::env::current_exe().ok()?.with_extension("old")).ok()?)
}

/// First "https://discord.com/api/webhooks/<id>/<token>" in a binary. The prefix alone also
/// appears as a plain string (link checks, hint text), so the id and token are required.
fn webhook_in(bytes: &[u8]) -> Option<String> {
    let pre = b"https://discord.com/api/webhooks/";
    bytes.windows(pre.len()).enumerate().filter(|(_, w)| w == pre).find_map(|(i, _)| {
        let rest = &bytes[i + pre.len()..];
        let id = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        // tokens are 68 characters; whatever string follows in the binary can continue the run
        let token = rest.get(id + 1..)?.iter().take(68).take_while(|b| b.is_ascii_alphanumeric() || **b == b'_' || **b == b'-').count();
        (id >= 17 && rest[id] == b'/' && token == 68).then(|| String::from_utf8_lossy(&bytes[i..i + pre.len() + id + 1 + token]).into())
    })
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
    fn webhook_from_old_build() {
        let url = format!("https://discord.com/api/webhooks/1497384162297122957/{}", "aB3_-".repeat(13) + "xyz");
        let bin = format!("\0https://discord.com/api/webhooks/https://discordapp.com/api/webhooks/x{url}Send crash log\0");
        assert_eq!(webhook_in(bin.as_bytes()), Some(url));
        assert_eq!(webhook_in(b"https://discord.com/api/webhooks/..."), None);
    }

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
        // a download that doesn't match GitHub's digest never replaces the exe
        let bad = Release {
            version: "9.9.9".into(),
            url: "https://raw.githubusercontent.com/rust-lang/rust/master/README.md".into(),
            sha256: Some("0".repeat(64)),
        };
        assert!(install_to(&bad, exe.clone(), &Log::default()).unwrap_err().contains("checksum"));
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "old build");
        let r = Release {
            version: "9.9.9".into(),
            url: "https://raw.githubusercontent.com/rust-lang/rust/master/README.md".into(),
            sha256: None,
        };
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
        let r = Release {
            version: "9.9.9".into(),
            url: "https://raw.githubusercontent.com/rust-lang/rust/master/README.md".into(),
            sha256: None,
        };
        install_to(&r, exe.clone(), &Log::default()).unwrap();
        assert!(std::fs::read_to_string(&exe).unwrap().contains("Rust"));
        assert!(exe.with_extension("old").exists());
    }
}
