//! Modrinth (public, keyless API) and CurseForge (cfwidget or the browser for slug -> id,
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

/// Modrinth search results per page.
pub const PAGE: usize = 40;
/// Modrinth sort orders: (API index, label).
pub const SORTS: [(&str, &str); 4] =
    [("relevance", "Relevance"), ("downloads", "Downloads"), ("updated", "Recently updated"), ("newest", "Newest")];

/// `kind` is "modpack" or "mod" (mods/plugins, filtered to ones that run server-side).
/// `sort` is an index from `SORTS`; `offset` skips that many results (paging).
pub fn modrinth_search(
    query: &str,
    kind: &str,
    mc: Option<&str>,
    f: Option<Flavor>,
    offset: usize,
    sort: &str,
) -> Result<Vec<Hit>, String> {
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
    let r = get_json(&format!("{MR}/search?limit={PAGE}&offset={offset}&index={sort}&query={}&facets={}", enc(query), enc(&facets)))?;
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
pub fn modrinth_install(id: &str, mc: &str, f: Flavor, dest: &Path, log: &Log) -> Result<(), String> {
    let mut i = Install::new(mc, f, dest, log);
    i.mr_project(id).map_err(|e| format!("{id}: {e}"))?;
    i.finish(id)
}

/// Install one specific Modrinth version (picked on the project page) plus its required dependencies.
pub fn modrinth_install_version(version_id: &str, mc: &str, f: Flavor, dest: &Path, log: &Log) -> Result<(), String> {
    let v = get_json(&format!("{MR}/version/{}", enc(version_id)))?;
    let mut i = Install::new(mc, f, dest, log);
    i.mr_version(&v)?;
    i.finish(v["name"].as_str().unwrap_or(version_id))
}

/// One mod install and the required mods it pulls in.
struct Install<'a> {
    mc: &'a str,
    f: Flavor,
    dest: &'a Path,
    log: &'a Log,
    seen: HashSet<String>,
    /// required dependencies that couldn't be installed, with the reason
    missing: Vec<String>,
    /// Modrinth project of each mod already in `dest`, looked up once
    installed: Option<Vec<(PathBuf, String)>>,
}

impl<'a> Install<'a> {
    fn new(mc: &'a str, f: Flavor, dest: &'a Path, log: &'a Log) -> Self {
        Install { mc, f, dest, log, seen: HashSet::new(), missing: vec![], installed: None }
    }

    /// A missing required mod is an error: the server usually won't start without it.
    fn finish(self, what: &str) -> Result<(), String> {
        if self.missing.is_empty() {
            return Ok(());
        }
        Err(format!(
            "{what} was added, but required mods it needs couldn't be: {}. Add them from their pages before starting the server.",
            self.missing.join("; ")
        ))
    }

    fn mr_project(&mut self, id: &str) -> Result<(), String> {
        let loaders = serde_json::to_string(loaders(self.f)).map_err(s)?;
        let game = enc(&format!("[\"{}\"]", self.mc));
        let vs = get_json(&format!("{MR}/project/{}/version?game_versions={game}&loaders={}", enc(id), enc(&loaders)))?;
        let v = pick_version(&vs).ok_or(format!("no {:?} version for Minecraft {}", self.f, self.mc))?;
        self.mr_version(v)
    }

    /// Before installing a project: the mods of it already in `dest`. Skips (returns None) a
    /// dependency that is already installed; the mod the user asked for replaces its old versions.
    fn existing(&self, top: bool, same: Vec<PathBuf>) -> Option<Vec<PathBuf>> {
        match same.iter().find(|p| p.extension().is_some_and(|x| x == "jar")) {
            Some(p) if !top => {
                push(self.log, format!("{} is already installed", p.file_name().unwrap_or_default().to_string_lossy()));
                None
            }
            _ => Some(same),
        }
    }

    /// Move the old versions of a just-installed mod to the old folder.
    fn replace(&self, old: Vec<PathBuf>, new: &Path) -> Result<(), String> {
        for p in old.into_iter().filter(|p| p != new) {
            push(
                self.log,
                format!(
                    "Moving the old version {} to {}",
                    p.file_name().unwrap_or_default().to_string_lossy(),
                    old_dir(self.dest).display()
                ),
            );
            shelve(&p)?;
        }
        Ok(())
    }

    fn mr_version(&mut self, v: &Value) -> Result<(), String> {
        let pid = v["project_id"].as_str().unwrap_or("").to_string();
        // nothing seen yet: this is the mod the user asked for, not a dependency
        let top = self.seen.is_empty();
        if !self.seen.insert(format!("mr:{pid}")) {
            return Ok(());
        }
        let installed = self.installed.get_or_insert_with(|| mr_projects(&mod_files(self.dest)).unwrap_or_default());
        let same = installed.iter().filter(|(_, p)| *p == pid).map(|(f, _)| f.clone()).collect();
        let Some(old) = self.existing(top, same) else { return Ok(()) };
        let file = primary_file(v).ok_or("version has no files")?;
        let name = url_file_name(file["filename"].as_str().unwrap_or("mod.jar"));
        push(self.log, format!("Installing {name}"));
        std::fs::create_dir_all(self.dest).map_err(s)?;
        download(file["url"].as_str().ok_or("no file url")?, &self.dest.join(&name))?;
        self.replace(old, &self.dest.join(&name))?;
        for d in v["dependencies"].as_array().into_iter().flatten().filter(|d| d["dependency_type"] == "required") {
            // the author's pinned version if there is one, otherwise the newest compatible one
            let r = match (d["version_id"].as_str(), d["project_id"].as_str()) {
                (Some(vid), _) => get_json(&format!("{MR}/version/{}", enc(vid))).and_then(|dv| self.mr_version(&dv)),
                (None, Some(pid)) => self.mr_project(pid),
                _ => continue,
            };
            if let Err(e) = r {
                let id = d["project_id"].as_str().or(d["version_id"].as_str()).unwrap_or("?");
                let title = get_json(&format!("{MR}/project/{}", enc(id))).ok().and_then(|p| p["title"].as_str().map(String::from));
                self.missing_dep(title.as_deref().unwrap_or(id), e);
            }
        }
        Ok(())
    }

    fn missing_dep(&mut self, name: &str, e: String) {
        push(self.log, format!("WARN required mod {name}: {e}"));
        self.missing.push(format!("{name} ({e})"));
    }

    /// `file`: a specific file id, otherwise the newest one for this server's version and loader.
    fn cf_mod(&mut self, pid: u64, file: Option<u64>) -> Result<(), String> {
        let top = self.seen.is_empty();
        if !self.seen.insert(format!("cf:{pid}")) {
            return Ok(());
        }
        let (mc, f) = (self.mc, self.f);
        let files = cf_files(pid, |fs| fs.iter().any(|x| file.map_or(cf_matches(x, mc, f), |id| x["id"] == id)))?;
        let file = match file {
            Some(id) => files.iter().find(|x| x["id"] == id),
            None => {
                let ok: Vec<_> = files.iter().filter(|x| cf_matches(x, mc, f)).collect();
                ok.iter().find(|x| x["releaseType"] == 1).or(ok.first()).copied()
            }
        }
        .ok_or(format!("no {f:?} file for Minecraft {mc}"))?;
        // Same project = a jar named like one of its files (without a key there's no fingerprint
        // lookup). ponytail: only the file pages fetched above are compared, so a very old
        // installed version can be missed.
        let names: HashSet<String> = files.iter().filter_map(|x| x["fileName"].as_str()).map(url_file_name).collect();
        let same = mod_files(self.dest)
            .into_iter()
            .filter(|p| names.contains(p.file_name().unwrap_or_default().to_string_lossy().trim_end_matches(".disabled")))
            .collect();
        let Some(old) = self.existing(top, same) else { return Ok(()) };
        let fid = file["id"].as_u64().unwrap_or(0);
        let url = cf_download_url(pid, fid)?;
        let name = url_file_name(&url);
        push(self.log, format!("Installing {name}"));
        std::fs::create_dir_all(self.dest).map_err(s)?;
        download(&url, &self.dest.join(&name))?;
        self.replace(old, &self.dest.join(&name))?;
        let deps = match get_json(&format!("{CF}/{pid}/files/{fid}/dependencies?pageSize=50")) {
            Ok(d) => d,
            Err(e) => {
                self.missing_dep("its list of required mods", e);
                return Ok(());
            }
        };
        for d in deps["data"].as_array().into_iter().flatten().filter(|d| d["type"] == "RequiredDependency") {
            let Some(id) = d["id"].as_u64() else { continue };
            if let Err(e) = self.cf_mod(id, None) {
                self.missing_dep(d["name"].as_str().unwrap_or("?"), e);
            }
        }
        Ok(())
    }
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

fn post_json(url: &str, body: Value) -> Result<Value, String> {
    ureq::post(url).header("User-Agent", UA).send_json(body).map_err(s)?.body_mut().with_config().limit(50 << 20).read_json().map_err(s)
}

/// Mods in `dir`, disabled ones (`.jar.disabled`) included.
pub fn mod_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && [".jar", ".jar.disabled"].iter().any(|x| p.to_string_lossy().ends_with(x)))
        .collect();
    v.sort();
    v
}

/// Where removed and replaced mods go (`old-mods` next to `mods`), so they can be restored.
pub fn old_dir(content: &Path) -> PathBuf {
    content.with_file_name(format!("old-{}", content.file_name().unwrap_or_default().to_string_lossy()))
}

/// Move a mod into the old folder instead of deleting it. Returns where it went.
pub fn shelve(file: &Path) -> Result<PathBuf, String> {
    let old = old_dir(file.parent().ok_or("bad mod path")?);
    std::fs::create_dir_all(&old).map_err(s)?;
    let to = old.join(file.file_name().unwrap_or_default());
    std::fs::rename(file, &to).map_err(s)?;
    Ok(to)
}

/// Put a mod from the old folder back into `content`. Whatever replaced it (same Modrinth
/// project) goes to the old folder in its place, so a restore can itself be undone.
pub fn restore(old: &Path, content: &Path, log: &Log) -> Result<(), String> {
    let name = old.file_name().unwrap_or_default();
    if let Some((_, pid)) = mr_projects(&[old.to_path_buf()]).unwrap_or_default().pop() {
        for (f, p) in mr_projects(&mod_files(content))? {
            if p == pid && f.file_name() != Some(name) {
                push(log, format!("Moving {} to {}", f.file_name().unwrap_or_default().to_string_lossy(), old_dir(content).display()));
                shelve(&f)?;
            }
        }
    }
    push(log, format!("Restoring {}", name.to_string_lossy()));
    std::fs::create_dir_all(content).map_err(s)?;
    std::fs::rename(old, content.join(name)).map_err(s)
}

/// Modrinth project id of each file Modrinth knows (matched by sha1).
fn mr_projects(files: &[PathBuf]) -> Result<Vec<(PathBuf, String)>, String> {
    let hashed: Vec<(String, &PathBuf)> = files.iter().filter_map(|p| Some((sha1_file(p)?, p))).collect();
    if hashed.is_empty() {
        return Ok(vec![]);
    }
    let hashes: Vec<&String> = hashed.iter().map(|(h, _)| h).collect();
    let r = post_json(&format!("{MR}/version_files"), serde_json::json!({ "hashes": hashes, "algorithm": "sha1" }))?;
    Ok(hashed.into_iter().filter_map(|(h, p)| Some((p.clone(), r[&h]["project_id"].as_str()?.to_string()))).collect())
}

pub fn sha1_file(p: &Path) -> Option<String> {
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
    let r = post_json(&format!("{MR}/version_files/update"), body)?;
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

/// Of these jar sha1s, the ones Modrinth knows as client-only (server_side "unsupported").
pub fn client_only_hashes(hashes: &[String]) -> Result<Vec<String>, String> {
    if hashes.is_empty() {
        return Ok(vec![]);
    }
    let versions = post_json(&format!("{MR}/version_files"), serde_json::json!({ "hashes": hashes, "algorithm": "sha1" }))?;
    let by_project: Vec<(&String, &str)> = hashes.iter().filter_map(|h| Some((h, versions[h]["project_id"].as_str()?))).collect();
    if by_project.is_empty() {
        return Ok(vec![]);
    }
    let ids = serde_json::to_string(&by_project.iter().map(|(_, p)| p).collect::<Vec<_>>()).map_err(s)?;
    let projects = get_json(&format!("{MR}/projects?ids={}", enc(&ids)))?;
    let client: Vec<&str> =
        projects.as_array().into_iter().flatten().filter(|p| p["server_side"] == "unsupported").filter_map(|p| p["id"].as_str()).collect();
    Ok(by_project.into_iter().filter(|(_, p)| client.contains(p)).map(|(h, _)| h.clone()).collect())
}

/// Move the old jar to the old folder (restorable from the Mods tab), then download the new one.
pub fn apply_mod_update(u: &ModUpdate, log: &Log) -> Result<(), String> {
    let dir = u.file.parent().ok_or("bad mod path")?;
    push(log, format!("Updating {} to {}", u.file.file_name().unwrap_or_default().to_string_lossy(), u.new_name));
    let old = shelve(&u.file)?;
    if let Err(e) = download(&u.url, &dir.join(&u.new_name)) {
        let _ = std::fs::rename(&old, &u.file);
        return Err(e);
    }
    Ok(())
}

// ---------- Required versions (Forge / NeoForge mods.toml) ----------

/// Compare versions numerically per dot-separated part (each part's leading digits; missing = 0).
fn cmp_version(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> {
        v.split('.').map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)).collect()
    };
    let (a, b) = (parts(a), parts(b));
    (0..a.len().max(b.len()))
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// Maven version range: `[a,b)`, `(a,]`, `[a]` and comma-separated unions of those. A bare
/// version or `*` is only a preference, so it accepts anything.
pub fn in_range(v: &str, range: &str) -> bool {
    use std::cmp::Ordering::*;
    let mut rest = range.trim();
    if !rest.starts_with(['[', '(']) {
        return true;
    }
    while let Some(start) = rest.find(['[', '(']) {
        let Some(len) = rest[start..].find([']', ')']) else { return true };
        let iv = &rest[start..=start + len];
        rest = &rest[start + len + 1..];
        let inner = &iv[1..iv.len() - 1];
        let ok = match inner.split_once(',') {
            None => cmp_version(v, inner.trim()) == Equal,
            Some((lo, hi)) => {
                let (lo, hi) = (lo.trim(), hi.trim());
                (lo.is_empty() || matches!((cmp_version(v, lo), iv.starts_with('[')), (Greater, _) | (Equal, true)))
                    && (hi.is_empty() || matches!((cmp_version(v, hi), iv.ends_with(']')), (Less, _) | (Equal, true)))
            }
        };
        if ok {
            return true;
        }
    }
    false
}

/// What a Forge / NeoForge mods.toml declares: (modId, version) and required (modId, versionRange).
#[derive(Default, Debug, PartialEq)]
struct ModsToml {
    mods: Vec<(String, String)>,
    deps: Vec<(String, String)>,
}

/// Line-based read of the few mods.toml keys needed. `jar_version` replaces `${file.jarVersion}`.
fn parse_mods_toml(t: &str, jar_version: Option<&str>) -> ModsToml {
    let mut tables: Vec<(&str, std::collections::HashMap<&str, String>)> = vec![];
    let mut multi: Option<&str> = None;
    for l in t.lines().map(str::trim) {
        if let Some(q) = multi {
            if l.contains(q) {
                multi = None;
            }
            continue;
        }
        if l.starts_with('[') {
            let kind = if l.starts_with("[[mods]]") {
                "mods"
            } else if l.starts_with("[[dependencies.") {
                "dep"
            } else {
                ""
            };
            tables.push((kind, Default::default()));
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        if let Some(q) = ["'''", "\"\"\""].into_iter().find(|q| v.starts_with(q)) {
            multi = (!v[3..].contains(q)).then_some(q);
            continue;
        }
        let v = match v.chars().next().filter(|c| ['"', '\''].contains(c)) {
            Some(q) => v[1..].split(q).next().unwrap_or(""),
            None => v.split('#').next().unwrap_or("").trim(),
        };
        if let Some((_, t)) = tables.last_mut() {
            t.insert(k.trim(), v.to_string());
        }
    }
    let mut out = ModsToml::default();
    for (kind, t) in &tables {
        let Some(id) = t.get("modId").filter(|i| !i.is_empty()) else { continue };
        let get = |k: &str| t.get(k).map_or("", |v| v.as_str());
        if *kind == "mods" {
            let v = get("version");
            let v = if v == "${file.jarVersion}" { jar_version.unwrap_or("") } else { v };
            out.mods.push((id.clone(), v.to_string()));
        } else if *kind == "dep" && (get("type").eq_ignore_ascii_case("required") || get("mandatory") == "true") {
            out.deps.push((id.clone(), get("versionRange").to_string()));
        }
    }
    out
}

fn jar_mods_toml(jar: &Path) -> Option<ModsToml> {
    use std::io::Read;
    let mut z = zip::ZipArchive::new(std::fs::File::open(jar).ok()?).ok()?;
    let mut text = |name: &str| {
        let mut t = String::new();
        z.by_name(name).ok().and_then(|mut f| f.read_to_string(&mut t).ok()).map(|_| t)
    };
    let toml = text("META-INF/neoforge.mods.toml").or_else(|| text("META-INF/mods.toml"))?;
    let manifest = text("META-INF/MANIFEST.MF").unwrap_or_default();
    let jar_version = manifest.lines().find_map(|l| l.strip_prefix("Implementation-Version:")).map(str::trim);
    Some(parse_mods_toml(&toml, jar_version))
}

/// Last resort after a pack import skipped files: a mod older than a version another mod requires
/// is replaced by the newest Modrinth version of the same file, if that one meets every range.
/// Problems only log warnings; the import goes on.
pub fn upgrade_requirements(mods: &Path, mc: &str, f: Flavor, log: &Log) {
    let jars: Vec<(PathBuf, ModsToml)> = mod_files(mods)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|x| x == "jar"))
        .filter_map(|p| Some((jar_mods_toml(&p)?, p)))
        .map(|(m, p)| (p, m))
        .collect();
    let installed: std::collections::HashMap<&str, (usize, &str)> =
        jars.iter().enumerate().flat_map(|(i, (_, m))| m.mods.iter().map(move |(id, v)| (id.as_str(), (i, v.as_str())))).collect();
    // jar index -> (range, why)
    let mut need: std::collections::BTreeMap<usize, Vec<(String, String)>> = Default::default();
    for (p, m) in &jars {
        for (dep, range) in &m.deps {
            if let Some(&(i, v)) = installed.get(dep.as_str())
                && !v.is_empty()
                && !in_range(v, range)
            {
                let why = format!("{} needs {dep} {range}", p.file_name().unwrap_or_default().to_string_lossy());
                need.entry(i).or_default().push((range.clone(), why));
            }
        }
    }
    if need.is_empty() {
        return;
    }
    let hashes: Vec<(usize, String)> = need.keys().filter_map(|&i| Some((i, sha1_file(&jars[i].0)?))).collect();
    let body = serde_json::json!({
        "hashes": hashes.iter().map(|(_, h)| h).collect::<Vec<_>>(),
        "algorithm": "sha1",
        "loaders": loaders(f),
        "game_versions": [mc],
    });
    let r = match post_json(&format!("{MR}/version_files/update"), body) {
        Ok(r) => r,
        Err(e) => {
            return push(
                log,
                format!("WARN couldn't look for newer versions of outdated mods on Modrinth ({e}). The server may not start."),
            );
        }
    };
    for (i, h) in hashes {
        let (old, why) = (&jars[i].0, need[&i].iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>().join(", "));
        let old_name = old.file_name().unwrap_or_default().to_string_lossy();
        let v = &r[&h];
        let pf = primary_file(v).filter(|pf| pf["hashes"]["sha1"] != h.as_str());
        let (Some(_), Some(pf)) = (v["version_number"].as_str().filter(|n| need[&i].iter().all(|(r, _)| in_range(n, r))), pf) else {
            push(log, format!("WARN {old_name} is too old ({why}) and Modrinth has no version that fits. The server may not start."));
            continue;
        };
        let (Some(url), Some(sha1), Some(name)) = (pf["url"].as_str(), pf["hashes"]["sha1"].as_str(), pf["filename"].as_str()) else {
            continue;
        };
        let to = mods.join(url_file_name(name));
        if let Err(e) = download(url, &to).and_then(|_| crate::verify(&to, sha1)) {
            push(log, format!("WARN couldn't download {name} to replace {old_name} ({e}). The server may not start."));
            continue;
        }
        if &to != old {
            let _ = std::fs::remove_file(old);
        }
        push(log, format!("Replaced {old_name} with {name} because {why}"));
    }
}

// ---------- CurseForge (no API key) ----------

pub struct CfLink {
    class: String,
    slug: String,
    file: Option<u64>,
}

/// curseforge.com search page for `class` ("mc-mods", "modpacks", "bukkit-plugins"), filtered to
/// the version and loader when given. For the user's browser: the site's search only answers
/// real browsers.
pub fn cf_search_url(query: &str, class: &str, mc: Option<&str>, f: Option<Flavor>) -> String {
    let mut u = format!("https://www.curseforge.com/minecraft/search?class={class}&sortBy=relevancy&search={}", enc(query.trim()));
    if let Some(mc) = mc {
        u += &format!("&version={}", enc(mc));
    }
    // CurseForge's loader ids
    let loader = match f {
        Some(Flavor::Forge) => Some(1),
        Some(Flavor::Fabric) => Some(4),
        Some(Flavor::NeoForge) => Some(6),
        _ => None,
    };
    if let Some(id) = loader {
        u += &format!("&gameVersionTypeId={id}");
    }
    u
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

pub(crate) fn cf_project_id(l: &CfLink) -> Result<u64, String> {
    if let Ok(project) = get_json(&format!("{CFWIDGET}/{}/{}", enc(&l.class), enc(&l.slug)))
        && let Some(id) = project["id"].as_u64().filter(|id| *id > 0)
    {
        return Ok(id);
    }
    crate::browser::project_id(&format!("https://www.curseforge.com/minecraft/{}/{}", enc(&l.class), enc(&l.slug)))
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

/// CurseForge tags each file with the sides it runs on; "Client" without "Server" = client-only.
/// Untagged files (or a failed lookup) count as not client-only.
pub fn cf_client_only(pid: u64, fid: u64) -> bool {
    get_json(&format!("{CF}/{pid}/files/{fid}")).is_ok_and(|v| {
        let tags = &v["data"]["gameVersions"];
        let has = |t: &str| tags.as_array().is_some_and(|a| a.iter().any(|x| x == t));
        has("Client") && !has("Server")
    })
}

/// CurseForge redirects downloads to edge.forgecdn.net, which 404s for many files;
/// mediafilez.forgecdn.net serves the same paths.
pub fn mediafilez(url: &str) -> String {
    url.replace("://edge.forgecdn.net/", "://mediafilez.forgecdn.net/")
}

/// Start of the error for a CurseForge file that can't be downloaded anywhere. Pack imports skip
/// these with a warning; any other error means a check failed and the import can be retried.
pub const CF_GONE: &str = "No longer on CurseForge";
/// Public mirror of the official CurseForge API (keyless). Its rules: fallback use only, with the
/// app's own User-Agent.
const MCIM: &str = "https://mod.mcimirror.top/curseforge/v1/mods";

/// What one source says about a CurseForge file.
#[derive(Debug, PartialEq)]
enum Cf {
    /// Download link and sha1 when known.
    Url(String, Option<String>),
    Gone,
    Unknown,
}

/// GET without following redirects or turning 4xx/5xx into errors: status, Location header, and
/// the body as JSON (Null if it isn't). None when there was no answer.
fn probe(url: &str) -> Option<(u16, Option<String>, Value)> {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().max_redirects(0).max_redirects_will_error(false).http_status_as_error(false).build().into();
    let mut r = agent.get(url).header("User-Agent", UA).call().ok()?;
    let loc = r.headers().get("location").and_then(|v| v.to_str().ok()).map(String::from);
    let body = r.body_mut().with_config().limit(50 << 20).read_to_string().ok()?;
    Some((r.status().as_u16(), loc, serde_json::from_str(&body).unwrap_or(Value::Null)))
}

/// 200 with JSON: whatever `found` makes of it, else gone. 404: gone. Anything else: unknown.
fn answer(p: Option<(u16, Option<String>, Value)>, found: impl FnOnce(&Value) -> Option<Cf>) -> Cf {
    match p {
        Some((200, _, v)) if !v.is_null() => found(&v).unwrap_or(Cf::Gone),
        Some((404, ..)) => Cf::Gone,
        _ => Cf::Unknown,
    }
}

/// MCIM's copy of the official file info. An unavailable file (author blocks other launchers)
/// has no usable link, so it's looked up on Modrinth by sha1, like Prism does.
fn mcim_answer(p: Option<(u16, Option<String>, Value)>, cdn: impl Fn(&str) -> String, modrinth: impl FnOnce(&str) -> Cf) -> Cf {
    answer(p, |v| {
        let d = &v["data"];
        let name = d["fileName"].as_str()?;
        let sha1 = d["hashes"].as_array()?.iter().find(|h| h["algo"] == 1).and_then(|h| h["value"].as_str());
        if d["isAvailable"] == true && d["downloadUrl"].is_string() {
            return Some(Cf::Url(cdn(name), sha1.map(String::from)));
        }
        Some(sha1.map_or(Cf::Gone, modrinth))
    })
}

fn modrinth_by_sha1(h: &str) -> Cf {
    answer(probe(&format!("{MR}/version_file/{h}?algorithm=sha1")), |v| {
        let f = v["files"].as_array()?.iter().find(|f| f["hashes"]["sha1"] == h)?;
        Some(Cf::Url(f["url"].as_str()?.to_string(), Some(h.to_string())))
    })
}

/// First link wins. Gone only when every source says so; one unanswered check makes it unknown.
fn settle(checks: impl Iterator<Item = Cf>, pid: u64, fid: u64) -> Result<(String, Option<String>), String> {
    let mut unsure = false;
    for c in checks {
        match c {
            Cf::Url(u, h) => return Ok((u, h)),
            Cf::Unknown => unsure = true,
            Cf::Gone => {}
        }
    }
    Err(if unsure {
        format!("Couldn't check CurseForge file {fid}. Check your internet connection and retry.")
    } else {
        format!("{CF_GONE}: file {fid} of project {pid} can't be downloaded from CurseForge and isn't on Modrinth")
    })
}

/// Download link and sha1 (when known) of a CurseForge file. Checks, in order: the download
/// redirect, the website's file info, MCIM (then Modrinth for files hidden from other
/// launchers), and cfwidget's file list.
pub fn cf_file(pid: u64, fid: u64) -> Result<(String, Option<String>), String> {
    // forgecdn paths are /files/<id / 1000>/<id % 1000>/<file name>
    let cdn = |name: &str| format!("https://mediafilez.forgecdn.net/files/{}/{}/{}", fid / 1000, fid % 1000, enc(name));
    let download = || match probe(&format!("{CF}/{pid}/files/{fid}/download")) {
        Some((_, Some(loc), _)) => Cf::Url(mediafilez(loc.split('?').next().unwrap_or(&loc)), None),
        Some((404, ..)) => Cf::Gone,
        _ => Cf::Unknown,
    };
    let site = || answer(probe(&format!("{CF}/{pid}/files/{fid}")), |v| Some(Cf::Url(cdn(v["data"]["fileName"].as_str()?), None)));
    let mcim = || mcim_answer(probe(&format!("{MCIM}/{pid}/files/{fid}")), cdn, modrinth_by_sha1);
    let widget = || {
        answer(probe(&format!("https://api.cfwidget.com/{pid}")), |v| {
            let f = v["files"].as_array()?.iter().find(|f| f["id"].as_u64() == Some(fid))?;
            Some(Cf::Url(cdn(f["name"].as_str()?), None))
        })
    };
    let checks: [&dyn Fn() -> Cf; 4] = [&download, &site, &mcim, &widget];
    settle(checks.iter().map(|c| c()), pid, fid)
}

pub fn cf_download_url(pid: u64, fid: u64) -> Result<String, String> {
    cf_file(pid, fid).map(|(u, _)| u)
}

fn cf_matches(file: &Value, mc: &str, f: Flavor) -> bool {
    let gv: Vec<String> = file["gameVersions"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(str::to_lowercase).collect();
    let loader_ok = f == Flavor::Paper || gv.iter().any(|g| loaders(f).contains(&g.as_str()));
    gv.iter().any(|g| g == mc) && loader_ok
}

/// Install a CurseForge mod/plugin from its page link into `dest`, plus its required mods.
pub fn cf_install_mod(l: &CfLink, mc: &str, f: Flavor, dest: &Path, log: &Log) -> Result<(), String> {
    let pid = cf_project_id(l)?;
    let mut i = Install::new(mc, f, dest, log);
    i.cf_mod(pid, l.file).map_err(|e| format!("{}: {e}", l.slug))?;
    i.finish(&l.slug)
}

/// Download a CurseForge modpack to `to`, preferring its Server Files pack.
/// Returns (flavor, mc) from the file's tags as a fallback hint.
/// `server_only`: refuse a pack without server files instead of downloading the client pack.
pub fn cf_modpack(l: &CfLink, to: &Path, server_only: bool, log: &Log) -> Result<Option<(Flavor, String)>, String> {
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
    if server_only && !server_pack {
        return Err(format!(
            "{} has no server pack on CurseForge, and Octo only downloads server packs. Download the pack yourself and add it under Import, which cleans it into a server.",
            target.1
        ));
    }
    push(log, format!("Downloading {} ({})", target.1, if server_pack { "server pack" } else { "no server pack, using client pack" }));
    download(&cf_download_url(pid, target.0)?, to)?;
    let gv: Vec<&str> = file["gameVersions"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
    let flavor = gv.iter().find_map(|g| flavor_from(g));
    let mc = gv.iter().find(|g| g.chars().next().is_some_and(|c| c.is_ascii_digit()));
    Ok(flavor.zip(mc.map(|m| m.to_string())))
}

// ---------- Technic and ATLauncher (public, keyless; modpacks only) ----------

// Technic's API wants the launcher build in build=; any recent number works
const TECHNIC: &str = "https://api.technicpack.net";
const ATL: &str = "https://api.atlauncher.com/v1";
/// ATLauncher's CDN; pack files are listed relative to it.
pub const ATL_CDN: &str = "https://download.nodecdn.net/containers/atl";

/// Technic search. Hits link to the pack page; Technic's search has no descriptions or counts.
pub fn technic_search(query: &str) -> Result<Vec<Hit>, String> {
    let r = get_json(&format!("{TECHNIC}/search?build=999&q={}", enc(query)))?;
    Ok(r["modpacks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Hit {
                title: m["name"].as_str()?.into(),
                slug: format!("https://www.technicpack.net/modpack/{}", m["slug"].as_str()?),
                description: String::new(),
                downloads: 0,
                icon_url: m["iconUrl"].as_str().unwrap_or("").into(),
            })
        })
        .collect())
}

/// `https://www.technicpack.net/modpack/<slug>[.<id>]`
pub fn parse_technic(link: &str) -> Option<String> {
    let seg = link.split("technicpack.net/modpack/").nth(1)?.split(['/', '?', '#']).next()?;
    let slug = match seg.rsplit_once('.') {
        Some((slug, id)) if id.chars().all(|c| c.is_ascii_digit()) => slug,
        _ => seg,
    };
    (!slug.is_empty()).then(|| slug.to_string())
}

/// The Technic pack's server files. Community packs often only have the client pack.
pub fn technic_server_zip(slug: &str) -> Result<String, String> {
    let m = get_json(&format!("{TECHNIC}/modpack/{}?build=999", enc(slug)))?;
    let name = m["displayName"].as_str().unwrap_or(slug);
    m["serverPackUrl"]
        .as_str()
        .filter(|u| !u.is_empty())
        .map(String::from)
        .ok_or(format!("{name} has no server files on Technic. Download its server pack from the pack's website, then use Import."))
}

/// ATLauncher's public packs whose name has every word of `query`, most recently updated first.
pub fn atl_search(query: &str) -> Result<Vec<Hit>, String> {
    let r = get_json(&format!("{ATL}/packs/full/public"))?;
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut packs: Vec<&Value> = r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| {
            let name = p["name"].as_str().unwrap_or("").to_lowercase();
            words.iter().all(|w| name.contains(w.as_str()))
        })
        .collect();
    let updated = |p: &Value| p["versions"].as_array().into_iter().flatten().filter_map(|v| v["published"].as_u64()).max();
    packs.sort_by_key(|p| std::cmp::Reverse(updated(p)));
    Ok(packs
        .into_iter()
        .filter_map(|p| {
            let safe = p["safeName"].as_str()?;
            let desc = p["description"].as_str().unwrap_or("").lines().next().unwrap_or("");
            Some(Hit {
                title: p["name"].as_str()?.into(),
                slug: format!("https://atlauncher.com/pack/{safe}"),
                description: if desc.chars().count() > 160 { desc.chars().take(160).collect::<String>() + "..." } else { desc.into() },
                downloads: 0,
                icon_url: format!("https://cdn.atlcdn.net/images/packs/{}.png", safe.to_lowercase()),
            })
        })
        .collect())
}

/// `https://atlauncher.com/pack/<safeName>`
pub fn parse_atl(link: &str) -> Option<String> {
    link.split("atlauncher.com/pack/").nth(1)?.split(['/', '?', '#']).next().filter(|s| !s.is_empty()).map(String::from)
}

/// Latest version of an ATLauncher pack and its install config (mods, loader, Minecraft version).
pub fn atl_config(safe: &str) -> Result<(String, Value), String> {
    let pack = get_json(&format!("{ATL}/pack/{}", enc(safe)))?;
    let ver = pack["data"]["versions"][0]["version"].as_str().ok_or(format!("ATLauncher pack {safe} has no versions"))?.to_string();
    let config = get_json(&format!("{ATL_CDN}/packs/{}/versions/{}/Configs.json", enc(safe), enc(&ver)))?;
    Ok((ver, config))
}

/// Add a mod/plugin from a CurseForge or Modrinth link, a Modrinth slug, or a direct .jar URL.
pub fn add_mod(src: &str, mc: &str, f: Flavor, server_dir: &Path, log: &Log) -> Result<(), String> {
    let dest: PathBuf = server_dir.join(content_dir(f));
    let src = src.trim();
    if let Some(l) = parse_cf(src) {
        cf_install_mod(&l, mc, f, &dest, log)
    } else if src.starts_with("http") && !src.contains("modrinth.com/") {
        std::fs::create_dir_all(&dest).map_err(s)?;
        let to = dest.join(url_file_name(src));
        download(src, &to)?;
        // a link to a download page saves HTML; a real mod is a zip
        if std::fs::read(&to).map_err(s)?.starts_with(b"PK") {
            return Ok(());
        }
        let _ = std::fs::remove_file(&to);
        Err("That link gave a web page, not a mod file. Open it in a browser, copy the direct download link of the .jar, and add that."
            .into())
    } else {
        let slug = parse_modrinth(src).map(|(s, _)| s).unwrap_or(src.to_string());
        modrinth_install(&slug, mc, f, &dest, log)
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
        assert!(old_dir(&d).join(ups[0].file.file_name().unwrap()).exists(), "old jar kept");
        assert!(check_mod_updates(&d, "1.21.1", Flavor::Fabric).unwrap().is_empty(), "up to date now");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Required dependencies come along from both sites.
    /// cargo test install_deps_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn install_deps_live() {
        let d = std::env::temp_dir().join(format!("octo-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let jars = |d: &Path| {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into()).collect();
            v.sort();
            v
        };
        modrinth_install("modmenu", "1.21.1", Flavor::Fabric, &d.join("mr"), &Log::default()).unwrap();
        let mr = jars(&d.join("mr"));
        println!("{mr:?}");
        assert!(mr.iter().any(|j| j.starts_with("fabric-api")), "{mr:?}");
        let l = parse_cf("https://www.curseforge.com/minecraft/mc-mods/applied-energistics-2").unwrap();
        cf_install_mod(&l, "1.21.1", Flavor::NeoForge, &d.join("cf"), &Log::default()).unwrap();
        let cf = jars(&d.join("cf"));
        println!("{cf:?}");
        assert!(cf.iter().any(|j| j.to_lowercase().contains("guideme")), "{cf:?}");
        let log = Log::default();
        cf_install_mod(&l, "1.21.1", Flavor::NeoForge, &d.join("cf"), &log).unwrap();
        assert_eq!(jars(&d.join("cf")), cf, "reinstall adds nothing");
        assert!(log.lock().unwrap().iter().any(|l| l.contains("guideme") && l.contains("already installed")));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Installing a mod again replaces its old version instead of adding a second jar, an
    /// already-installed dependency isn't downloaded again, and a restore swaps back.
    /// cargo test replace_and_restore_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn replace_and_restore_live() {
        let d = std::env::temp_dir().join(format!("octo-replace-{}", std::process::id())).join("mods");
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
        let log = Log::default();
        let vs = get_json(&format!("{MR}/project/modmenu/version?game_versions=%5B%221.21.1%22%5D")).unwrap();
        let oldest = vs.as_array().unwrap().last().unwrap();
        modrinth_install_version(oldest["id"].as_str().unwrap(), "1.21.1", Flavor::Fabric, &d, &log).unwrap();
        let first = mod_files(&d);
        modrinth_install("modmenu", "1.21.1", Flavor::Fabric, &d, &log).unwrap();
        let now = mod_files(&d);
        let names = |v: &[PathBuf]| v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect::<Vec<_>>();
        println!("{:?} -> {:?}, old: {:?}", names(&first), names(&now), names(&mod_files(&old_dir(&d))));
        assert_eq!(names(&now).iter().filter(|n| n.starts_with("modmenu")).count(), 1);
        assert_eq!(now.len(), first.len(), "dependencies not doubled");
        assert!(log.lock().unwrap().iter().any(|l| l.contains("is already installed")));
        let old = mod_files(&old_dir(&d));
        assert_eq!(old.len(), 1);
        restore(&old[0], &d, &log).unwrap();
        assert!(mod_files(&d).contains(&d.join(old[0].file_name().unwrap())), "old version back");
        assert_eq!(mod_files(&old_dir(&d)).len(), 1, "the newer one swapped out");
        std::fs::remove_dir_all(d.parent().unwrap()).unwrap();
    }

    #[test]
    fn missing_deps_fail_the_install() {
        let log = Log::default();
        let mut i = Install::new("1.21.1", Flavor::Fabric, Path::new("/nowhere"), &log);
        assert!(Install::new("1.21.1", Flavor::Fabric, Path::new("/nowhere"), &log).finish("x").is_ok());
        i.missing_dep("Fabric API", "no Fabric version for Minecraft 1.21.1".into());
        let e = i.finish("Mod Menu").unwrap_err();
        assert!(e.starts_with("Mod Menu was added, but") && e.contains("Fabric API (no Fabric version"), "{e}");
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
        assert_eq!(
            cf_search_url("just enough", "mc-mods", Some("1.21.1"), Some(Flavor::NeoForge)),
            "https://www.curseforge.com/minecraft/search?class=mc-mods&sortBy=relevancy&search=just%20enough&version=1.21.1&gameVersionTypeId=6"
        );
        assert!(!cf_search_url("", "bukkit-plugins", None, Some(Flavor::Paper)).contains("gameVersionTypeId"));
        assert_eq!(parse_technic("https://www.technicpack.net/modpack/tekkit.552560"), Some("tekkit".into()));
        assert_eq!(parse_technic("https://www.technicpack.net/modpack/hexxit"), Some("hexxit".into()));
        assert_eq!(parse_atl("https://atlauncher.com/pack/AllTheForge10?x=1"), Some("AllTheForge10".into()));
        assert!(parse_technic("https://modrinth.com/mod/x").is_none() && parse_atl("https://modrinth.com/mod/x").is_none());
    }

    #[test]
    fn version_ranges() {
        assert!(!in_range("1.113.1", "[1.119.0,)"));
        assert!(in_range("1.120.2", "[1.119.0,)"));
        assert!(in_range("1.119.0", "[1.119.0,)") && !in_range("1.119.0", "(1.119.0,)"));
        assert!(in_range("2.0", "(1.0,2.0]") && !in_range("2.0", "(1.0,2.0)") && !in_range("1.0", "(1.0,2.0]"));
        assert!(in_range("1.5", "[1.5]") && !in_range("1.5.1", "[1.5]"));
        assert!(in_range("1.0.0", "[1.0]"), "missing parts count as 0");
        assert!(in_range("3.1", "[1,2),[3,)") && !in_range("2.5", "[1,2),[3,)"));
        assert!(in_range("0.1", "1.5") && in_range("0.1", "*") && in_range("0.1", ""));
        assert!(in_range("1.10", "[1.9,)"), "numeric, not text, comparison");
        assert!(in_range("21.1.65-beta", "[21.1.0,)"));
    }

    #[test]
    fn reads_mods_toml() {
        let t = r#"
modLoader="javafml"
[[mods]] #main
modId="advancedperipherals"
version="${file.jarVersion}"
description='''
modId="fake"
'''
[[dependencies.advancedperipherals]]
    modId="computercraft"
    type="required" # needed
    versionRange="[1.119.0,)"
[[dependencies.advancedperipherals]]
    modId='jei'
    type="optional"
    versionRange="[1,)"
[[dependencies.advancedperipherals]]
    modId="minecraft"
    mandatory=true
    versionRange="[1.21.1]"
"#;
        let m = parse_mods_toml(t, Some("0.7.41"));
        assert_eq!(m.mods, vec![("advancedperipherals".into(), "0.7.41".into())]);
        assert_eq!(m.deps, vec![("computercraft".into(), "[1.119.0,)".into()), ("minecraft".into(), "[1.21.1]".into())]);
    }

    #[test]
    fn hidden_files_vs_unknown() {
        let cdn = |n: &str| format!("cdn/{n}");
        let ok = |v: Value| Some((200, None, v));
        let file = |avail: bool| {
            serde_json::json!({"data": {"fileName": "a.jar", "isAvailable": avail, "downloadUrl": "x",
                "hashes": [{"value": "md5", "algo": 2}, {"value": "abc", "algo": 1}]}})
        };
        let mr = |h: &str| Cf::Url(format!("mr/{h}"), Some(h.into()));
        assert_eq!(mcim_answer(ok(file(true)), cdn, mr), Cf::Url("cdn/a.jar".into(), Some("abc".into())));
        assert_eq!(mcim_answer(ok(file(false)), cdn, mr), Cf::Url("mr/abc".into(), Some("abc".into())));
        assert_eq!(mcim_answer(ok(file(false)), cdn, |_| Cf::Gone), Cf::Gone);
        assert_eq!(mcim_answer(ok(serde_json::json!({"data": null})), cdn, mr), Cf::Gone);
        assert_eq!(mcim_answer(Some((404, None, Value::Null)), cdn, mr), Cf::Gone);
        assert_eq!(mcim_answer(Some((502, None, Value::Null)), cdn, mr), Cf::Unknown);
        assert_eq!(mcim_answer(None, cdn, mr), Cf::Unknown);
        assert_eq!(mcim_answer(Some((200, None, Value::Null)), cdn, mr), Cf::Unknown, "not JSON, like a block page");

        assert!(settle([Cf::Gone, Cf::Gone].into_iter(), 1, 2).unwrap_err().starts_with(CF_GONE));
        let e = settle([Cf::Gone, Cf::Unknown, Cf::Gone].into_iter(), 1, 2).unwrap_err();
        assert!(!e.starts_with(CF_GONE) && e.contains("retry"), "{e}");
        assert_eq!(settle([Cf::Unknown, Cf::Url("u".into(), None)].into_iter(), 1, 2), Ok(("u".into(), None)));
    }

    /// ATM10's CC Tweaked Compat is hidden from other launchers; MCIM gives its sha1, which
    /// Modrinth knows as the official CC: Tweaked.
    /// cargo test hidden_cf_file_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn hidden_cf_file_live() {
        let (url, sha1) = cf_file(1676502, 8778582).unwrap();
        println!("{url}");
        assert_eq!(url_file_name(&url), "cc-tweaked-1.21.1-forge-1.120.2.jar");
        assert!(url.starts_with("https://cdn.modrinth.com/"), "{url}");
        assert_eq!(sha1.as_deref(), Some("f35d4f636c2eb1b30c5d3f5c08e110c6acb8e02f"));
    }
}
