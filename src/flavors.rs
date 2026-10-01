use crate::{Log, cmd, download, get_json, get_text, push, run_logged, s};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Flavor {
    Vanilla,
    Paper,
    Fabric,
    Forge,
    NeoForge,
}

impl Flavor {
    pub const ALL: [Flavor; 5] = [Flavor::Vanilla, Flavor::Paper, Flavor::Fabric, Flavor::Forge, Flavor::NeoForge];
}

const MOJANG: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const PAPER: &str = "https://fill.papermc.io/v3/projects/paper";
const FABRIC: &str = "https://meta.fabricmc.net/v2/versions";
const FORGE_PROMOS: &str = "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json";
const FORGE_MAVEN: &str = "https://maven.minecraftforge.net/net/minecraftforge/forge";
const NEO_MAVEN: &str = "https://maven.neoforged.net/releases/net/neoforged/neoforge";

fn vkey(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn sort_desc(mut v: Vec<String>) -> Vec<String> {
    v.sort_by_key(|x| std::cmp::Reverse(vkey(x)));
    v.dedup();
    v
}

fn str_list(v: &serde_json::Value, pred: impl Fn(&serde_json::Value) -> bool, key: &str) -> Vec<String> {
    v.as_array().into_iter().flatten().filter(|x| pred(x)).filter_map(|x| x[key].as_str().map(String::from)).collect()
}

/// Minecraft versions available for a flavor, newest first.
pub fn versions(f: Flavor) -> Result<Vec<String>, String> {
    Ok(match f {
        Flavor::Vanilla => str_list(&get_json(MOJANG)?["versions"], |x| x["type"] == "release", "id"),
        Flavor::Paper => {
            let p = get_json(PAPER)?;
            let all = p["versions"].as_object().ok_or("bad Paper response")?.values();
            let flat = all.flat_map(|g| g.as_array().cloned().unwrap_or_default());
            sort_desc(flat.filter_map(|v| v.as_str().map(String::from)).filter(|v| !v.contains('-')).collect())
        }
        Flavor::Fabric => str_list(&get_json(&format!("{FABRIC}/game"))?, |x| x["stable"] == true, "version"),
        Flavor::Forge => {
            let p = get_json(FORGE_PROMOS)?;
            let keys = p["promos"].as_object().ok_or("bad Forge response")?.keys();
            let mcs = keys.filter_map(|k| k.rsplit_once('-')).map(|(mc, _)| mc.to_string());
            // ponytail: pre-1.7.10 Forge has no --installServer installer; add a legacy path if anyone asks.
            sort_desc(mcs.filter(|mc| vkey(mc) >= vec![1, 7, 10]).collect())
        }
        Flavor::NeoForge => sort_desc(neo_versions()?.iter().map(|v| neo_mc(v)).collect()),
    })
}

/// Java major version Mojang says this MC version needs.
pub fn java_major(mc: &str) -> Result<u32, String> {
    let m = get_json(MOJANG)?;
    let Some(url) = m["versions"].as_array().into_iter().flatten().find(|v| v["id"] == mc).and_then(|v| v["url"].as_str()) else {
        return Ok(21);
    };
    Ok(get_json(url)?["javaVersion"]["majorVersion"].as_u64().unwrap_or(8) as u32)
}

fn neo_versions() -> Result<Vec<String>, String> {
    let xml = get_text(&format!("{NEO_MAVEN}/maven-metadata.xml"))?;
    Ok(xml.split("<version>").skip(1).filter_map(|s| s.split("</version>").next()).map(String::from).collect())
}

/// MC version -> NeoForge version prefix. `1.21.1`->`21.1.`, `1.21`->`21.0.`, `26.1`->`26.1.0.`, `26.1.2`->`26.1.2.`
fn neo_prefix(mc: &str) -> String {
    match mc.strip_prefix("1.") {
        Some(r) => {
            let mut p = r.split('.');
            format!("{}.{}.", p.next().unwrap_or("0"), p.next().unwrap_or("0"))
        }
        None => {
            let mut p = mc.split('.');
            format!("{}.{}.{}.", p.next().unwrap_or("0"), p.next().unwrap_or("0"), p.next().unwrap_or("0"))
        }
    }
}

/// Inverse of `neo_prefix`.
pub fn neo_mc(neo: &str) -> String {
    let p: Vec<&str> = neo.split('-').next().unwrap_or(neo).split('.').collect();
    match p.len() {
        4.. if p[2] == "0" => format!("{}.{}", p[0], p[1]),
        4.. => format!("{}.{}.{}", p[0], p[1], p[2]),
        _ if p.get(1) == Some(&"0") => format!("1.{}", p[0]),
        _ => format!("1.{}.{}", p[0], p.get(1).unwrap_or(&"0")),
    }
}

/// Download/install the server into `dir`. `java` is used to run Forge/NeoForge installers.
/// `loader` pins the Fabric/Forge/NeoForge version (modpacks need an exact one); None = newest/recommended.
pub fn install(f: Flavor, mc: &str, loader: Option<&str>, dir: &Path, java: &Path, log: &Log) -> Result<(), String> {
    let jar = dir.join("server.jar");
    match f {
        Flavor::Vanilla => {
            let m = get_json(MOJANG)?;
            let url = m["versions"].as_array().into_iter().flatten().find(|v| v["id"] == mc).and_then(|v| v["url"].as_str());
            let meta = get_json(url.ok_or("unknown version")?)?;
            let url = meta["downloads"]["server"]["url"].as_str().ok_or("this version has no server jar")?;
            push(log, format!("Downloading {url}"));
            download(url, &jar)
        }
        Flavor::Paper => {
            let builds = get_json(&format!("{PAPER}/versions/{mc}/builds"))?;
            let list = builds.as_array().ok_or("no Paper builds")?;
            let b = list.iter().find(|b| b["channel"] == "STABLE").or(list.first()).ok_or("no Paper builds")?;
            let url = b["downloads"]["server:default"]["url"].as_str().ok_or("bad Paper build")?;
            push(log, format!("Downloading Paper build {}", b["id"]));
            download(url, &jar)
        }
        Flavor::Fabric => {
            let first_stable = |what: &str| -> Result<String, String> {
                let l = str_list(&get_json(&format!("{FABRIC}/{what}"))?, |x| x["stable"] == true, "version");
                l.into_iter().next().ok_or(format!("no stable Fabric {what}"))
            };
            let loader = match loader {
                Some(l) => l.to_string(),
                None => first_stable("loader")?,
            };
            let installer = first_stable("installer")?;
            push(log, format!("Downloading Fabric loader {loader}"));
            download(&format!("{FABRIC}/loader/{mc}/{loader}/{installer}/server/jar"), &jar)
        }
        Flavor::Forge => {
            let p = get_json(FORGE_PROMOS)?;
            let v = loader
                .or(p["promos"][format!("{mc}-recommended")].as_str())
                .or(p["promos"][format!("{mc}-latest")].as_str())
                .ok_or("no Forge build for this version")?;
            // Forge 1.7.10-1.9.4 builds carry an extra "-<mc>" suffix in their maven name.
            let urls =
                [format!("{mc}-{v}"), format!("{mc}-{v}-{mc}")].map(|full| format!("{FORGE_MAVEN}/{full}/forge-{full}-installer.jar"));
            run_installer(&urls, dir, java, log)
        }
        Flavor::NeoForge => {
            let all = neo_versions()?;
            let newest = all.iter().rev().find(|v| v.starts_with(&neo_prefix(mc))).map(String::as_str);
            let v = loader.or(newest).ok_or("no NeoForge build for this version")?;
            run_installer(&[format!("{NEO_MAVEN}/{v}/neoforge-{v}-installer.jar")], dir, java, log)
        }
    }
}

/// Downloads the first of `urls` that exists, then runs it with --installServer.
fn run_installer(urls: &[String], dir: &Path, java: &Path, log: &Log) -> Result<(), String> {
    let inst = dir.join("installer.jar");
    let mut last = Err("no installer url".to_string());
    for url in urls {
        push(log, format!("Downloading {url}"));
        last = download(url, &inst);
        if last.is_ok() {
            break;
        }
    }
    last?;
    let mut c = cmd(java);
    c.args(["-jar", "installer.jar", "--installServer"]).current_dir(dir);
    run_logged(c, log)?;
    let _ = std::fs::remove_file(&inst);
    let _ = std::fs::remove_file(dir.join("installer.jar.log"));
    Ok(())
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().is_some_and(|n| n == name) {
            return Some(p);
        }
    }
    None
}

/// Arguments after `java -Xmx..`.
pub fn launch_args(f: Flavor, dir: &Path) -> Result<Vec<String>, String> {
    if !matches!(f, Flavor::Forge | Flavor::NeoForge) {
        return Ok(vec!["-jar".into(), "server.jar".into(), "nogui".into()]);
    }
    // Modern Forge/NeoForge (1.17+): an @argfile under libraries/.
    let args_name = if cfg!(windows) { "win_args.txt" } else { "unix_args.txt" };
    if let Some(p) = find_file(&dir.join("libraries"), args_name) {
        let rel = p.strip_prefix(dir).map_err(s)?;
        return Ok(vec![format!("@{}", rel.display()), "nogui".into()]);
    }
    // Old Forge: a forge-*.jar in the server folder.
    let jar = std::fs::read_dir(dir)
        .map_err(s)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .find(|n| n.starts_with("forge-") && n.ends_with(".jar") && !n.contains("installer"))
        .ok_or("The Forge server jar is missing. Try creating the server again.")?;
    Ok(vec!["-jar".into(), jar, "nogui".into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neoforge_mapping() {
        for (mc, prefix) in [("1.21.1", "21.1."), ("1.21", "21.0."), ("26.1", "26.1.0."), ("26.1.2", "26.1.2.")] {
            assert_eq!(neo_prefix(mc), prefix);
        }
        assert_eq!(neo_mc("21.1.84-beta"), "1.21.1");
        assert_eq!(neo_mc("21.0.167"), "1.21");
        assert_eq!(neo_mc("26.3.0.39-beta"), "26.3");
        assert_eq!(neo_mc("26.1.2.5"), "26.1.2");
        for v in ["21.1.84-beta", "21.0.167", "26.3.0.39-beta", "26.1.2.5"] {
            assert!(v.starts_with(&neo_prefix(&neo_mc(v))), "{v}");
        }
    }

    #[test]
    fn version_sort() {
        let v = sort_desc(vec!["1.20.1".into(), "26.1".into(), "1.21.11".into(), "1.21.2".into(), "1.21.2".into()]);
        assert_eq!(v, ["26.1", "1.21.11", "1.21.2", "1.20.1"]);
    }
}

#[cfg(test)]
mod e2e {
    use super::*;
    #[test]
    #[ignore]
    fn install_all() {
        let root = PathBuf::from(std::env::var("E2E_DIR").unwrap());
        for f in Flavor::ALL {
            let vs = versions(f).unwrap();
            let mc = &vs[0];
            let jm = java_major(mc).unwrap();
            let java = crate::java::find(jm).expect("java");
            let dir = root.join(format!("{f:?}"));
            std::fs::create_dir_all(&dir).unwrap();
            let log = Log::default();
            let r = install(f, mc, None, &dir, &java, &log);
            println!("{f:?} {mc} java{jm} {} -> {:?} args={:?}", vs.len(), r, launch_args(f, &dir));
            if r.is_err() {
                println!("{}", log.lock().unwrap().join("\n"));
            }
        }
    }
}
