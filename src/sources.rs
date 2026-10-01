//! Modrinth (public, keyless API) and CurseForge (crawled: cfwidget for slug -> id,
//! curseforge.com's own file-list/download endpoints, forgecdn for the files).
use crate::flavors::Flavor;
use crate::{Log, UA, download, enc, get_json, push, s, url_file_name};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MR: &str = "https://api.modrinth.com/v2";
const CF: &str = "https://www.curseforge.com/api/v1/mods";
const CFWIDGET: &str = "https://api.cfwidget.com/minecraft";

#[derive(Clone)]
pub struct Hit {
    pub title: String,
    pub slug: String,
    pub description: String,
    pub downloads: u64,
    pub icon_url: String,
}

/// Modrinth loader names that run on this server type.
pub fn loaders(f: Flavor) -> &'static [&'static str] {
    match f {
        Flavor::Fabric => &["fabric"],
        Flavor::Forge => &["forge"],
        Flavor::NeoForge => &["neoforge"],
        Flavor::Paper => &["paper", "bukkit", "spigot"],
        Flavor::Vanilla => &[],
    }
}

/// Where add-ons live for this server type.
pub fn content_dir(f: Flavor) -> &'static str {
    if f == Flavor::Paper { "plugins" } else { "mods" }
}

pub fn flavor_from(name: &str) -> Option<Flavor> {
    match name.to_ascii_lowercase().as_str() {
        "fabric" | "fabric-loader" => Some(Flavor::Fabric),
        "forge" => Some(Flavor::Forge),
        "neoforge" => Some(Flavor::NeoForge),
        "paper" => Some(Flavor::Paper),
        "vanilla" => Some(Flavor::Vanilla),
        _ => None,
    }
}

// ---------- Modrinth ----------

/// `kind` is "modpack" or "mod" (mods/plugins, filtered to ones that run server-side).
pub fn modrinth_search(query: &str, kind: &str, mc: Option<&str>, f: Option<Flavor>) -> Result<Vec<Hit>, String> {
    let mut facets: Vec<Vec<String>> = vec![vec![format!("project_type:{}", if f == Some(Flavor::Paper) { "plugin" } else { kind })]];
    if let Some(mc) = mc {
        facets.push(vec![format!("versions:{mc}")]);
    }
    if let Some(f) = f.filter(|f| !loaders(*f).is_empty()) {
        facets.push(loaders(f).iter().map(|l| format!("categories:{l}")).collect());
    }
    if kind != "modpack" {
        facets.push(vec!["server_side:required".into(), "server_side:optional".into()]);
    }
    let facets = serde_json::to_string(&facets).map_err(s)?;
    let r = get_json(&format!("{MR}/search?limit=40&query={}&facets={}", enc(query), enc(&facets)))?;
    Ok(r["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| Hit {
            title: h["title"].as_str().unwrap_or("?").into(),
            slug: h["slug"].as_str().unwrap_or("").into(),
            description: h["description"].as_str().unwrap_or("").into(),
            downloads: h["downloads"].as_u64().unwrap_or(0),
            icon_url: h["icon_url"].as_str().unwrap_or("").into(),
        })
        .collect())
}

fn pick_version(vs: &Value) -> Option<&Value> {
    let a = vs.as_array()?;
    a.iter().find(|v| v["version_type"] == "release").or(a.first())
}

fn primary_file(v: &Value) -> Option<&Value> {
    let files = v["files"].as_array()?;
    files.iter().find(|f| f["primary"] == true).or(files.first())
}

/// Install a Modrinth mod/plugin (plus its required dependencies) into `dest`.
pub fn modrinth_install(id: &str, mc: &str, f: Flavor, dest: &Path, log: &Log, seen: &mut HashSet<String>) -> Result<(), String> {
    let loaders = serde_json::to_string(loaders(f)).map_err(s)?;
    let vs =
        get_json(&format!("{MR}/project/{}/version?game_versions={}&loaders={}", enc(id), enc(&format!("[\"{mc}\"]")), enc(&loaders)))?;
    let v = pick_version(&vs).ok_or(format!("{id} has no {f:?} version for Minecraft {mc}"))?;
    install_version(v, mc, f, dest, log, seen)
}

/// Install one specific Modrinth version (picked on the project page) plus its required dependencies.
pub fn modrinth_install_version(version_id: &str, mc: &str, f: Flavor, dest: &Path, log: &Log) -> Result<(), String> {
    let v = get_json(&format!("{MR}/version/{}", enc(version_id)))?;
    install_version(&v, mc, f, dest, log, &mut HashSet::new())
}

fn install_version(v: &Value, mc: &str, f: Flavor, dest: &Path, log: &Log, seen: &mut HashSet<String>) -> Result<(), String> {
    if !seen.insert(v["project_id"].as_str().unwrap_or("").to_string()) {
        return Ok(());
    }
    let file = primary_file(v).ok_or("version has no files")?;
    let name = url_file_name(file["filename"].as_str().unwrap_or("mod.jar"));
    push(log, format!("Installing {name}"));
    std::fs::create_dir_all(dest).map_err(s)?;
    download(file["url"].as_str().ok_or("no file url")?, &dest.join(&name))?;
    for d in v["dependencies"].as_array().into_iter().flatten().filter(|d| d["dependency_type"] == "required") {
        if let Some(pid) = d["project_id"].as_str()
            && let Err(e) = modrinth_install(pid, mc, f, dest, log, seen)
        {
            push(log, format!("WARN dependency {pid}: {e}"));
        }
    }
    Ok(())
}

/// Download a Modrinth modpack's .mrpack (newest release, or `version` id/number) to `to`.
pub fn modrinth_modpack(slug: &str, version: Option<&str>, to: &Path, log: &Log) -> Result<(), String> {
    let vs = get_json(&format!("{MR}/project/{}/version", enc(slug)))?;
    let v = match version {
        Some(want) => vs.as_array().into_iter().flatten().find(|v| v["id"] == want || v["version_number"] == want),
        None => pick_version(&vs),
    }
    .ok_or(format!("no versions found for {slug}"))?;
    let file = primary_file(v).ok_or("version has no files")?;
    push(log, format!("Downloading {}", file["filename"].as_str().unwrap_or("")));
    download(file["url"].as_str().ok_or("no file url")?, to)
}

/// `https://modrinth.com/<type>/<slug>[/version/<v>]` -> (slug, version)
pub fn parse_modrinth(link: &str) -> Option<(String, Option<String>)> {
    let rest = link.split("modrinth.com/").nth(1)?;
    let p: Vec<&str> = rest.split(['?', '#']).next()?.split('/').filter(|x| !x.is_empty()).collect();
    let slug = p.get(1)?.to_string();
    Some((slug, p.get(2).filter(|x| **x == "version").and(p.get(3)).map(|v| v.to_string())))
}

/// A mod jar with a newer Modrinth version for this server's loader and Minecraft version.
#[derive(Clone)]
pub struct ModUpdate {
    pub file: PathBuf,
    pub new_name: String,
    pub url: String,
}

fn sha1_file(p: &Path) -> Option<String> {
    Some(sha1_smol::Sha1::from(std::fs::read(p).ok()?).digest().to_string())
}

/// Ask Modrinth, by file fingerprint, which jars in `dir` have newer compatible versions.
/// Jars Modrinth doesn't know (CurseForge-only, custom builds) are skipped.
pub fn check_mod_updates(dir: &Path, mc: &str, f: Flavor) -> Result<Vec<ModUpdate>, String> {
    let jars: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map_err(s)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jar"))
        .filter_map(|p| Some((sha1_file(&p)?, p)))
        .collect();
    if jars.is_empty() {
        return Ok(vec![]);
    }
    let body = serde_json::json!({
        "hashes": jars.iter().map(|(h, _)| h).collect::<Vec<_>>(),
        "algorithm": "sha1",
        "loaders": loaders(f),
        "game_versions": [mc],
    });
    let r: Value = ureq::post(format!("{MR}/version_files/update"))
        .header("User-Agent", UA)
        .send_json(body)
        .map_err(s)?
        .body_mut()
        .with_config()
        .limit(50 << 20)
        .read_json()
        .map_err(s)?;
    Ok(jars
        .into_iter()
        .filter_map(|(hash, file)| {
            let pf = primary_file(&r[&hash])?;
            // the newest compatible version may be the one already installed
            if pf["hashes"]["sha1"].as_str()? == hash {
                return None;
            }
            Some(ModUpdate { file, new_name: url_file_name(pf["filename"].as_str()?), url: pf["url"].as_str()?.to_string() })
        })
        .collect())
}

/// Download the new jar next to the old one, then remove the old one.
pub fn apply_mod_update(u: &ModUpdate, log: &Log) -> Result<(), String> {
    let dir = u.file.parent().ok_or("bad mod path")?;
    push(log, format!("Updating {} to {}", u.file.file_name().unwrap_or_default().to_string_lossy(), u.new_name));
    download(&u.url, &dir.join(&u.new_name))?;
    if dir.join(&u.new_name) != u.file {
        std::fs::remove_file(&u.file).map_err(s)?;
    }
    Ok(())
}

// ---------- CurseForge (no API key) ----------

pub struct CfLink {
    class: String,
    slug: String,
    file: Option<u64>,
}

/// `https://www.curseforge.com/minecraft/<class>/<slug>[/files/<id>|/download/<id>]`
pub fn parse_cf(link: &str) -> Option<CfLink> {
    let rest = link.split("curseforge.com/minecraft/").nth(1)?;
    let p: Vec<&str> = rest.split(['?', '#']).next()?.split('/').filter(|x| !x.is_empty()).collect();
    Some(CfLink {
        class: p.first()?.to_string(),
        slug: p.get(1)?.to_string(),
        file: p.get(3).filter(|_| matches!(p.get(2), Some(&"files") | Some(&"download"))).and_then(|f| f.parse().ok()),
    })
}

fn cf_project_id(l: &CfLink) -> Result<u64, String> {
    // cfwidget answers "queued" for projects it hasn't cached yet; retry a bit.
    for _ in 0..6 {
        if let Some(id) = get_json(&format!("{CFWIDGET}/{}/{}", enc(&l.class), enc(&l.slug)))?["id"].as_u64() {
            return Ok(id);
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
    Err(format!("couldn't look up CurseForge project {}", l.slug))
}

/// Newest-first file list; stops early once `done` is satisfied.
fn cf_files(pid: u64, done: impl Fn(&[Value]) -> bool) -> Result<Vec<Value>, String> {
    let mut all = vec![];
    for page in 0..20 {
        let r =
            get_json(&format!("{CF}/{pid}/files?pageIndex={page}&pageSize=50&sort=dateCreated&sortDescending=true&removeAlphas=false"))?;
        let data = r["data"].as_array().cloned().unwrap_or_default();
        let last = data.len() < 50;
        all.extend(data);
        if last || done(&all) {
            break;
        }
    }
    Ok(all)
}

/// CurseForge redirects downloads to edge.forgecdn.net, which 404s for many files;
/// mediafilez.forgecdn.net serves the same paths.
pub fn cf_download_url(pid: u64, fid: u64) -> Result<String, String> {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().max_redirects(0).max_redirects_will_error(false).http_status_as_error(false).build().into();
    let r = agent.get(format!("{CF}/{pid}/files/{fid}/download")).header("User-Agent", UA).call().map_err(s)?;
    let loc = r.headers().get("location").and_then(|v| v.to_str().ok()).ok_or(format!("CurseForge file {fid} not downloadable"))?;
    Ok(loc.split('?').next().unwrap_or(loc).replace("://edge.forgecdn.net/", "://mediafilez.forgecdn.net/"))
}

fn cf_matches(file: &Value, mc: &str, f: Flavor) -> bool {
    let gv: Vec<String> = file["gameVersions"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_lowercase).collect();
    let loader_ok = f == Flavor::Paper || gv.iter().any(|g| loaders(f).contains(&g.as_str()));
    gv.iter().any(|g| g == mc) && loader_ok
}

/// Install a CurseForge mod/plugin from its page link into `dest`.
pub fn cf_install_mod(l: &CfLink, mc: &str, f: Flavor, dest: &Path, log: &Log) -> Result<(), String> {
    let pid = cf_project_id(l)?;
    let files = cf_files(pid, |fs| fs.iter().any(|x| cf_matches(x, mc, f)))?;
    let file = match l.file {
        Some(id) => files.iter().find(|x| x["id"] == id),
        None => {
            let ok: Vec<_> = files.iter().filter(|x| cf_matches(x, mc, f)).collect();
            ok.iter().find(|x| x["releaseType"] == 1).or(ok.first()).copied()
        }
    }
    .ok_or(format!("{} has no {f:?} file for Minecraft {mc}", l.slug))?;
    let url = cf_download_url(pid, file["id"].as_u64().unwrap_or(0))?;
    let name = url_file_name(&url);
    push(log, format!("Installing {name}"));
    std::fs::create_dir_all(dest).map_err(s)?;
    download(&url, &dest.join(name))
}

/// Download a CurseForge modpack to `to`, preferring its Server Files pack.
/// Returns (flavor, mc) from the file's tags as a fallback hint.
pub fn cf_modpack(l: &CfLink, to: &Path, log: &Log) -> Result<Option<(Flavor, String)>, String> {
    let pid = cf_project_id(l)?;
    let files = cf_files(pid, |fs| l.file.is_none_or(|id| fs.iter().any(|x| x["id"] == id)) && !fs.is_empty())?;
    let file = match l.file {
        Some(id) => files.iter().find(|x| x["id"] == id),
        None => files.iter().find(|x| x["releaseType"] == 1).or(files.first()),
    }
    .ok_or("modpack has no files")?;
    let fid = file["id"].as_u64().unwrap_or(0);
    let mut target = (fid, file["displayName"].as_str().unwrap_or("").to_string());
    if file["hasServerPack"] == true {
        let extra = get_json(&format!("{CF}/{pid}/files/{fid}/additional-files"))?;
        let sp = extra["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["isServerPack"] == true || x["fileName"].as_str().unwrap_or("").to_lowercase().contains("server"));
        if let Some(sp) = sp {
            target = (sp["id"].as_u64().unwrap_or(0), sp["displayName"].as_str().unwrap_or("").to_string());
        }
    }
    let server_pack = target.0 != fid || file["isServerPack"] == true;
    push(log, format!("Downloading {} ({})", target.1, if server_pack { "server pack" } else { "no server pack, using client pack" }));
    download(&cf_download_url(pid, target.0)?, to)?;
    let gv: Vec<&str> = file["gameVersions"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
    let flavor = gv.iter().find_map(|g| flavor_from(g));
    let mc = gv.iter().find(|g| g.chars().next().is_some_and(|c| c.is_ascii_digit()));
    Ok(flavor.zip(mc.map(|m| m.to_string())))
}

/// Add a mod/plugin from a CurseForge or Modrinth link, a Modrinth slug, or a direct .jar URL.
pub fn add_mod(src: &str, mc: &str, f: Flavor, server_dir: &Path, log: &Log) -> Result<(), String> {
    let dest: PathBuf = server_dir.join(content_dir(f));
    let src = src.trim();
    if let Some(l) = parse_cf(src) {
        cf_install_mod(&l, mc, f, &dest, log)
    } else if src.starts_with("http") && !src.contains("modrinth.com/") {
        std::fs::create_dir_all(&dest).map_err(s)?;
        download(src, &dest.join(url_file_name(src)))
    } else {
        let slug = parse_modrinth(src).map(|(s, _)| s).unwrap_or(src.to_string());
        modrinth_install(&slug, mc, f, &dest, log, &mut HashSet::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An old Fabric API jar should be offered (and get) the newest 1.21.1 build.
    /// cargo test mod_updates_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn mod_updates_live() {
        let d = std::env::temp_dir().join(format!("octo-modupd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let vs = get_json(&format!("{MR}/project/fabric-api/version?game_versions=%5B%221.21.1%22%5D")).unwrap();
        let oldest = vs.as_array().unwrap().last().unwrap();
        let f = primary_file(oldest).unwrap();
        download(f["url"].as_str().unwrap(), &d.join(f["filename"].as_str().unwrap())).unwrap();
        std::fs::write(d.join("not-on-modrinth.jar"), "x").unwrap();
        let ups = check_mod_updates(&d, "1.21.1", Flavor::Fabric).unwrap();
        assert_eq!(ups.len(), 1, "only the Modrinth jar gets an update");
        println!("{} -> {}", ups[0].file.display(), ups[0].new_name);
        apply_mod_update(&ups[0], &Log::default()).unwrap();
        assert!(!ups[0].file.exists() && d.join(&ups[0].new_name).exists());
        assert!(check_mod_updates(&d, "1.21.1", Flavor::Fabric).unwrap().is_empty(), "up to date now");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn links() {
        let l = parse_cf("https://www.curseforge.com/minecraft/modpacks/all-the-mods-9/files/7097953").unwrap();
        assert_eq!((l.class.as_str(), l.slug.as_str(), l.file), ("modpacks", "all-the-mods-9", Some(7097953)));
        let l = parse_cf("https://www.curseforge.com/minecraft/mc-mods/jei?x=1").unwrap();
        assert_eq!((l.slug.as_str(), l.file), ("jei", None));
        assert_eq!(parse_modrinth("https://modrinth.com/modpack/adrenaserver"), Some(("adrenaserver".into(), None)));
        assert_eq!(parse_modrinth("https://modrinth.com/mod/sodium/version/abc"), Some(("sodium".into(), Some("abc".into()))));
        assert!(parse_cf("https://modrinth.com/mod/x").is_none());
    }
}
