use crate::{Log, cmd, push, run_logged};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Major version from a JDK/JRE `release` file (`JAVA_VERSION="21.0.4"`, `"1.8.0_412"`).
pub fn parse_release(s: &str) -> Option<u32> {
    let v = s.lines().find_map(|l| l.strip_prefix("JAVA_VERSION="))?.trim().trim_matches('"');
    let v = v.strip_prefix("1.").unwrap_or(v);
    v.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Major version from `java -version` output (`openjdk version "17.0.20.1"`, `java version "1.8.0_412"`).
pub fn parse_version_output(s: &str) -> Option<u32> {
    let v = s.split('"').nth(1)?;
    parse_release(&format!("JAVA_VERSION=\"{v}\""))
}

/// Vendor subfolder names under Program Files / LOCALAPPDATA\Programs that hold Javas.
const WINDOWS_VENDORS: &[&str] = &[
    "Eclipse Adoptium",
    "Java",
    "Microsoft",
    "Zulu",
    "Amazon Corretto",
    "BellSoft",
    "Semeru",
    "AdoptOpenJDK",
    "OpenJDK",
    "Eclipse Foundation",
    "ojdkbuild",
];

/// Find Java homes (dirs containing `bin/<exe>`) under `roots`, at most `max_depth` levels deep.
/// Never fails on missing/unreadable dirs - just yields nothing for that branch.
fn scan(roots: &[PathBuf], exe: &str, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        scan_dir(root, exe, max_depth, &mut found);
    }
    found
}

fn scan_dir(dir: &std::path::Path, exe: &str, depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join("bin").join(exe).is_file() {
        found.push(dir.to_path_buf());
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, exe, depth - 1, found);
        }
    }
}

/// Installed Javas: major version -> path to the java binary.
pub fn detect() -> BTreeMap<u32, PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let mut homes: Vec<PathBuf> = Vec::new();

    if cfg!(windows) {
        let mut vendor_roots: Vec<PathBuf> = Vec::new();
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(pf) = std::env::var(var) {
                vendor_roots.extend(WINDOWS_VENDORS.iter().map(|v| PathBuf::from(&pf).join(v)));
            }
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let programs = PathBuf::from(local).join("Programs");
            vendor_roots.extend(WINDOWS_VENDORS.iter().map(|v| programs.join(v)));
        }
        homes.extend(scan(&vendor_roots, exe, 4));
    } else {
        homes.extend(scan(&[PathBuf::from("/usr/lib/jvm")], exe, 4));
    }

    // Every directory on PATH that itself contains the java binary.
    if let Some(p) = std::env::var_os("PATH") {
        homes.extend(
            std::env::split_paths(&p).filter(|d| d.join(exe).is_file()).filter_map(|d| d.parent().map(std::path::Path::to_path_buf)),
        );
    }

    homes.extend(std::env::var_os("JAVA_HOME").map(PathBuf::from));

    // Dedupe by canonical path so the same install found via multiple roots isn't probed twice.
    let mut seen = std::collections::HashSet::new();
    homes.retain(|h| seen.insert(std::fs::canonicalize(h).unwrap_or_else(|_| h.clone())));

    homes
        .into_iter()
        .filter_map(|h| {
            let java = h.join("bin").join(exe);
            if !java.exists() {
                return None;
            }
            // JDKs have a `release` file; plain JREs (e.g. Arch's jreNN-openjdk-headless) don't, so ask java itself.
            let major = match std::fs::read_to_string(h.join("release")) {
                Ok(r) => parse_release(&r)?,
                Err(_) => parse_version_output(&String::from_utf8_lossy(&cmd(&java).arg("-version").output().ok()?.stderr))?,
            };
            Some((major, java))
        })
        .collect()
}

/// Java 16 (MC 1.17) isn't packaged anywhere anymore; 17 runs it fine.
pub fn package_major(required: u32) -> u32 {
    if required == 16 { 17 } else { required }
}

pub fn find(required: u32) -> Option<PathBuf> {
    let j = detect();
    j.get(&required).or(j.get(&package_major(required))).cloned()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pm {
    Winget,
    Pacman,
    Apt,
    Dnf,
}

pub fn install_cmd(pm: Pm, required: u32) -> Vec<String> {
    let n = package_major(required);
    let v = match pm {
        Pm::Winget => format!(
            "winget install -e --id EclipseAdoptium.Temurin.{n}.JRE --silent --accept-package-agreements --accept-source-agreements"
        ),
        // A GUI can't type a sudo password; pkexec shows a graphical prompt.
        Pm::Pacman => format!("pkexec pacman -S --noconfirm --needed jre{n}-openjdk-headless"),
        Pm::Apt => format!("pkexec apt-get install -y openjdk-{n}-jre-headless"),
        Pm::Dnf => format!("pkexec dnf install -y java-{n}-openjdk-headless"),
    };
    v.split(' ').map(String::from).collect()
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file() || d.join(format!("{name}.exe")).is_file()))
}

fn detect_pm() -> Option<Pm> {
    if cfg!(windows) {
        return Some(Pm::Winget);
    }
    [(Pm::Pacman, "pacman"), (Pm::Apt, "apt-get"), (Pm::Dnf, "dnf")].into_iter().find(|(_, n)| on_path(n)).map(|(p, _)| p)
}

/// Return the java for `required`, installing it via the system package manager if missing.
pub fn ensure(required: u32, log: &Log) -> Result<PathBuf, String> {
    if let Some(j) = find(required) {
        return Ok(j);
    }
    let pm = detect_pm().ok_or("No supported package manager found (winget, pacman, apt, dnf). Install Java manually.")?;
    let args = install_cmd(pm, required);
    push(log, format!("Java {required} not found, running: {}", args.join(" ")));
    let mut c = cmd(&args[0]);
    c.args(&args[1..]);
    // winget returns non-zero for "already installed"; trust detection instead of the exit code.
    if let Err(e) = run_logged(c, log) {
        push(log, format!("installer {e}"));
    }
    find(required).ok_or(format!("Java {} still not found after install", package_major(required)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_parsing() {
        assert_eq!(parse_release("IMPLEMENTOR=x\nJAVA_VERSION=\"21.0.4\"\n"), Some(21));
        assert_eq!(parse_release("JAVA_VERSION=\"1.8.0_412\""), Some(8));
        assert_eq!(parse_release("JAVA_VERSION=\"17\""), Some(17));
        assert_eq!(parse_release("JAVA_VERSION=\"25.0.4.1\""), Some(25));
        assert_eq!(parse_release("nothing"), None);
        assert_eq!(parse_version_output("openjdk version \"17.0.20.1\" 2026-08-18\nOpenJDK Runtime"), Some(17));
        assert_eq!(parse_version_output("java version \"1.8.0_412\""), Some(8));
        assert_eq!(parse_version_output("openjdk version \"21\" 2023-09-19\nOpenJDK Runtime"), Some(21));
    }

    #[test]
    fn install_commands() {
        assert_eq!(install_cmd(Pm::Winget, 21)[4], "EclipseAdoptium.Temurin.21.JRE");
        assert_eq!(install_cmd(Pm::Pacman, 16).last().unwrap(), "jre17-openjdk-headless");
        assert_eq!(install_cmd(Pm::Apt, 8).last().unwrap(), "openjdk-8-jre-headless");
        assert_eq!(install_cmd(Pm::Dnf, 25).last().unwrap(), "java-25-openjdk-headless");
    }

    /// Builds a fake Windows-style Program Files tree:
    /// - Eclipse Adoptium/jre-17.0.9.9-hotspot/bin/java.exe  (+ release)   -> depth 1, found
    /// - Zulu/zulu-21/jre/bin/java.exe                       (+ release)   -> depth 2, found
    /// - TooDeep/a/b/c/d/e/bin/java.exe                                    -> depth 5, NOT found (max_depth 4)
    ///
    /// `tag` keeps parallel tests from sharing (and deleting) each other's tree.
    fn fake_tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("octo_java_scan_test_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let adoptium = root.join("Eclipse Adoptium").join("jre-17.0.9.9-hotspot");
        std::fs::create_dir_all(adoptium.join("bin")).unwrap();
        std::fs::write(adoptium.join("bin").join("java.exe"), b"").unwrap();
        std::fs::write(adoptium.join("release"), "JAVA_VERSION=\"17.0.9\"\n").unwrap();

        let zulu = root.join("Zulu").join("zulu-21").join("jre");
        std::fs::create_dir_all(zulu.join("bin")).unwrap();
        std::fs::write(zulu.join("bin").join("java.exe"), b"").unwrap();
        std::fs::write(zulu.join("release"), "JAVA_VERSION=\"21.0.1\"\n").unwrap();

        let too_deep = root.join("TooDeep").join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(too_deep.join("bin")).unwrap();
        std::fs::write(too_deep.join("bin").join("java.exe"), b"").unwrap();

        root
    }

    #[test]
    fn scan_finds_nested_homes_within_depth_limit() {
        let root = fake_tree("depth");
        let roots = vec![root.join("Eclipse Adoptium"), root.join("Zulu"), root.join("TooDeep")];

        let found = scan(&roots, "java.exe", 4);

        assert!(found.contains(&root.join("Eclipse Adoptium").join("jre-17.0.9.9-hotspot")));
        assert!(found.contains(&root.join("Zulu").join("zulu-21").join("jre")));
        assert!(!found.iter().any(|h| h.ends_with("e")), "too-deep home must not be found: {found:?}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_then_parse_release_gives_right_majors() {
        let root = fake_tree("majors");
        let roots = vec![root.join("Eclipse Adoptium"), root.join("Zulu")];

        let mut majors: Vec<u32> = scan(&roots, "java.exe", 4)
            .into_iter()
            .filter_map(|h| parse_release(&std::fs::read_to_string(h.join("release")).ok()?))
            .collect();
        majors.sort();

        assert_eq!(majors, vec![17, 21]);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn scan_ignores_missing_roots() {
        // A nonexistent root must not panic or error - just contribute nothing.
        let found = scan(&[PathBuf::from("/no/such/path/octo-java-test")], "java", 4);
        assert!(found.is_empty());
    }
}
