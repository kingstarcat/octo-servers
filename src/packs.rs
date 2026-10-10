//! Turn a modpack / server-files zip (from a link, URL or local file) into a server folder.
use crate::flavors::{Flavor, neo_mc};
use crate::sources::{self, flavor_from};
use crate::{Log, download_all, push, s};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub type Detected = (Flavor, String, Option<String>);

/// Error `import` gives for a client launcher instance when `clean` is off; the GUI then asks
/// whether to clean it.
pub const CLIENT_PACK: &str = "This is the client (launcher) version of the pack, not server files. \
    Download the pack's server version instead; it usually has Server in the file name.";

/// `over` = type + MC version the user picked manually (wins over detection).
/// `clean` = import a client launcher instance, leaving out its client-only mods.
/// `server_only`: from a download tab, which only installs server packs; a client pack is refused
/// there instead of offered for cleaning.
pub fn import(
    src: &str,
    dir: &Path,
    over: Option<(Flavor, String)>,
    clean: bool,
    server_only: bool,
    log: &Log,
) -> Result<Detected, String> {
    let src = src.trim().trim_matches('"');
    if let Some(safe) = sources::parse_atl(src) {
        let (f, mc, loader) = atl_pack(&safe, dir, log)?;
        return Ok(match over {
            Some((of, omc)) if (of, omc.as_str()) != (f, mc.as_str()) => (of, omc, None),
            _ => (f, mc, loader),
        });
    }
    let zip = dir.join("_download.zip");
    let mut hint = None;
    if let Some(l) = sources::parse_cf(src) {
        hint = sources::cf_modpack(&l, &zip, server_only, log)?;
    } else if let Some((slug, ver)) = sources::parse_modrinth(src) {
        sources::modrinth_modpack(&slug, ver.as_deref(), &zip, log)?;
    } else if let Some(slug) = sources::parse_technic(src) {
        let url = sources::technic_server_zip(&slug)?;
        push(log, format!("Downloading {url}"));
        crate::download(&url, &zip)?;
    } else if src.starts_with("http://") || src.starts_with("https://") {
        push(log, format!("Downloading {src}"));
        crate::download(src, &zip)?;
    } else if Path::new(src).is_dir() {
        // an already-unzipped server folder: copy it so the original stays untouched
        let root = server_root(Path::new(src));
        push(log, format!("Copying {}...", root.display()));
        let n = copy_dir(&root, dir)?;
        push(log, format!("Copied {n} files"));
    } else {
        std::fs::copy(src, &zip).map_err(|e| format!("{src}: {e}"))?;
    }
    if zip.exists() {
        extract(&zip, dir, log)?;
        std::fs::remove_file(&zip).map_err(s)?;
    }

    // A launcher instance (MultiMC / Prism / ATLauncher export) or a CurseForge pack without
    // server files is the *client* pack: it has client-only mods and no server files, so only
    // import it once the user agrees to clean it.
    let instance = ["mmc-pack.json", "instance.cfg", "instance.json"].iter().any(|f| dir.join(f).exists());
    let manifest = cf_manifest(dir);
    if (instance || manifest.is_some()) && !clean {
        return Err(if server_only { NOT_SERVER_PACK.into() } else { CLIENT_PACK.into() });
    }
    let detected = if instance {
        Some(crate::worlds::clean_instance(dir, log)?)
    } else if dir.join("modrinth.index.json").exists() {
        Some(mrpack(dir, log)?)
    } else if let Some(m) = manifest {
        Some(cf_client_pack(dir, &m, log)?)
    } else {
        detect(dir)
    };
    if let Some((f, mc)) = over {
        // keep the pack's exact loader version if it agrees with the manual choice
        let loader = detected.filter(|d| d.0 == f && d.1 == mc).and_then(|d| d.2);
        return Ok((f, mc, loader));
    }
    detected
        .or(hint.map(|(f, mc)| (f, mc, None)))
        .ok_or("Couldn't tell what kind of server this is. Pick the type and Minecraft version manually.".into())
}

/// Windows "Extract All" (and many zips) wrap everything in one extra folder, e.g.
/// `server\GT_New_Horizons_…\`; step into lone wrapper folders to reach the real server root.
fn server_root(src: &Path) -> PathBuf {
    let mut root = src.to_path_buf();
    loop {
        let entries: Vec<_> = std::fs::read_dir(&root).into_iter().flatten().flatten().collect();
        match entries.as_slice() {
            [only] if only.path().is_dir() && !ROOT_DIRS.iter().any(|r| only.file_name() == *r) => root = only.path(),
            _ => return root,
        }
    }
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<usize, String> {
    let mut n = 0;
    for e in std::fs::read_dir(src).map_err(s)?.flatten() {
        let (from, to) = (e.path(), dst.join(e.file_name()));
        if from.is_dir() {
            std::fs::create_dir_all(&to).map_err(s)?;
            n += copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| format!("{}: {e}", from.display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Ready-to-run packs (e.g. GT New Horizons) ship a start script whose `java …` line only
/// references files already in the folder. Returns that line's arguments (minus java itself,
/// -Xms/-Xmx so our RAM setting applies, and "$@"-style passthroughs). None = needs a loader install.
pub fn ready_launch(dir: &Path) -> Option<Vec<String>> {
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sh" || x == "bat" || x == "cmd"))
        .collect();
    // Prefer this OS's script: modern Forge's .sh points at unix_args.txt, whose ':' classpath breaks on Windows.
    let native = |p: &PathBuf| p.extension().is_some_and(|x| (x == "sh") != cfg!(windows));
    scripts.sort_by_key(|p| (!native(p), p.clone()));
    for script in scripts {
        let Ok(text) = std::fs::read_to_string(&script) else { continue };
        for line in text.lines() {
            let toks: Vec<String> = line.split_whitespace().map(|t| t.trim_matches(['"', '\'']).to_string()).collect();
            let Some(i) = toks.iter().position(|t| {
                let b = t.rsplit(['/', '\\']).next().unwrap_or(t).to_ascii_lowercase();
                b == "java" || b == "java.exe" || b == "javaw" || b == "javaw.exe"
            }) else {
                continue;
            };
            let args: Vec<String> = toks[i + 1..]
                .iter()
                .filter(|t| !t.starts_with("-Xms") && !t.starts_with("-Xmx") && !matches!(t.as_str(), "$@" | "%*" | "&&" | "pause"))
                .cloned()
                .collect();
            if args.iter().any(|a| a.contains('$') || a.contains('%')) {
                continue; // depends on script variables we can't resolve
            }
            let exists = |f: &str| dir.join(f).is_file();
            let jar = args.iter().position(|a| a == "-jar").and_then(|j| args.get(j + 1));
            let argfiles: Vec<&str> = args.iter().filter_map(|a| a.strip_prefix('@')).collect();
            let runnable = jar.map_or(!argfiles.is_empty(), |j| exists(j)) && argfiles.iter().all(|f| exists(f));
            if runnable {
                let mut args = args;
                if !args.iter().any(|a| a == "nogui" || a == "--nogui") {
                    args.push("nogui".into());
                }
                return Some(args);
            }
        }
    }
    None
}

/// Whether the launch args need a modern Java (module flags only exist on Java 9+), e.g.
/// GTNH runs Minecraft 1.7.10 on Java 17-25.
pub fn needs_modern_java(dir: &Path, args: &[String]) -> bool {
    let has = |s: &str| s.contains("--add-opens") || s.contains("--add-exports");
    args.iter().any(|a| has(a) || a.strip_prefix('@').and_then(|f| std::fs::read_to_string(dir.join(f)).ok()).is_some_and(|c| has(&c)))
}

/// Join a relative path from an untrusted pack, refusing anything that escapes `dir`.
fn safe_join(dir: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    p.components().all(|c| matches!(c, Component::Normal(_))).then(|| dir.join(p))
}

/// Folders that mean "this is already the server root", so don't strip them as a wrapper folder.
const ROOT_DIRS: &[&str] = &["mods", "config", "libraries", "plugins", "world", "overrides", "defaultconfigs", "kubejs"];

pub fn extract(zip: &Path, dir: &Path, log: &Log) -> Result<(), String> {
    let mut a = zip::ZipArchive::new(std::fs::File::open(zip).map_err(s)?).map_err(|e| format!("not a zip: {e}"))?;
    // enclosed_name() rejects absolute paths and `..` (zip-slip).
    let names: Vec<Option<PathBuf>> = (0..a.len()).map(|i| a.by_index_raw(i).ok().and_then(|f| f.enclosed_name())).collect();
    let first = |p: &PathBuf| p.components().next().map(|c| c.as_os_str().to_owned());
    let wrapper = names.iter().flatten().next().and_then(first).filter(|w| {
        !ROOT_DIRS.iter().any(|r| w == *r)
            && names.iter().flatten().all(|n| first(n).as_ref() == Some(w) && n.components().count() > 1 || n == Path::new(w))
    });
    push(log, format!("Extracting {} files...", a.len()));
    let mut lost = vec![];
    for (i, name) in names.iter().enumerate() {
        let Some(name) = name else { continue };
        let rel = match &wrapper {
            Some(w) => name.strip_prefix(w).unwrap_or(name),
            None => name,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dir.join(rel);
        let mut f = a.by_index(i).map_err(s)?;
        let r = if f.is_dir() {
            std::fs::create_dir_all(&out)
        } else {
            out.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|_| std::io::copy(&mut f, &mut std::fs::File::create(&out)?).map(|_| ()))
        };
        // e.g. a name Windows can't hold (':' '?' …): skipping a readme is fine, a jar is not
        if let Err(e) = r {
            push(log, format!("WARN skipped {}: {e}", rel.display()));
            if rel.extension().is_some_and(|x| x == "jar") {
                lost.push(rel.display().to_string());
            }
        }
    }
    if lost.is_empty() {
        return Ok(());
    }
    Err(format!("{} couldn't be extracted, and the server needs them. Check there's enough disk space, then try again.", lost.join(", ")))
}

/// Move everything in `src` into `dst` (merging folders), then remove `src`.
fn move_into(src: &Path, dst: &Path) -> Result<(), String> {
    for e in std::fs::read_dir(src).map_err(s)?.flatten() {
        let target = dst.join(e.file_name());
        if e.path().is_dir() && target.is_dir() {
            move_into(&e.path(), &target)?;
        } else {
            if target.is_file() {
                std::fs::remove_file(&target).map_err(s)?;
            }
            std::fs::rename(e.path(), &target).map_err(s)?;
        }
    }
    std::fs::remove_dir_all(src).map_err(s)
}

/// A download tab found a client pack (Octo only downloads server packs).
pub const NOT_SERVER_PACK: &str = "This download is the client version of the pack, and Octo only downloads server packs. Download the pack yourself and add it under Import, which cleans it into a server.";

/// Start of the error when pack files are missing; the GUI offers a retry for it.
pub const DOWNLOAD_FAILED: &str = "Some of the pack's files couldn't be downloaded";

/// A pack missing files usually won't start, so that fails the import.
fn finish_downloads(failed: Vec<String>) -> Result<(), String> {
    if failed.is_empty() {
        return Ok(());
    }
    let more = if failed.len() > 5 { format!(" and {} more", failed.len() - 5) } else { String::new() };
    Err(format!(
        "{DOWNLOAD_FAILED} ({}{more}). Check your internet connection and retry the import.",
        failed.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
    ))
}

/// Modrinth .mrpack: download server-side files, apply overrides.
fn mrpack(dir: &Path, log: &Log) -> Result<Detected, String> {
    let idx: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("modrinth.index.json")).map_err(s)?).map_err(s)?;
    let deps = &idx["dependencies"];
    let mc = deps["minecraft"].as_str().ok_or("modpack has no Minecraft version")?.to_string();
    if deps.get("quilt-loader").is_some() {
        return Err("Quilt modpacks aren't supported".into());
    }
    let (flavor, loader) = ["fabric-loader", "forge", "neoforge"]
        .iter()
        .find_map(|k| Some((flavor_from(k)?, deps[*k].as_str()?.to_string())))
        .map_or((Flavor::Vanilla, None), |(f, l)| (f, Some(l)));
    let mut jobs = vec![];
    for f in idx["files"].as_array().into_iter().flatten() {
        if f["env"]["server"] == "unsupported" {
            continue; // client-only mod
        }
        let path = f["path"].as_str().unwrap_or("");
        let to = safe_join(dir, path).ok_or(format!("modpack has an unsafe path: {path}"))?;
        if let Some(url) = f["downloads"][0].as_str() {
            jobs.push((url.to_string(), to, f["hashes"]["sha1"].as_str().map(String::from)));
        }
    }
    push(log, format!("Downloading {} server-side files...", jobs.len()));
    finish_downloads(download_all(jobs, log))?;
    for o in ["overrides", "server-overrides"] {
        if dir.join(o).is_dir() {
            move_into(&dir.join(o), dir)?;
        }
    }
    let _ = std::fs::remove_dir_all(dir.join("client-overrides"));
    let _ = std::fs::remove_file(dir.join("modrinth.index.json"));
    Ok((flavor, mc, loader))
}

/// ATLauncher pack: the latest version's server-side mods and its configs. ATLauncher has no
/// server packs, but its config marks each mod client and/or server.
fn atl_pack(safe: &str, dir: &Path, log: &Log) -> Result<Detected, String> {
    let (ver, c) = sources::atl_config(safe)?;
    push(log, format!("ATLauncher pack {safe} {ver}"));
    let mc = c["minecraft"].as_str().ok_or("the pack has no Minecraft version")?.to_string();
    let (flavor, loader) = match c["loader"]["type"].as_str() {
        Some(t) => {
            (flavor_from(t).ok_or(format!("{t} modpacks aren't supported"))?, c["loader"]["metadata"]["version"].as_str().map(String::from))
        }
        None => (Flavor::Vanilla, None),
    };
    let (jobs, mut failed) = atl_jobs(&c, &mc, dir, log)?;
    push(log, format!("Downloading {} server-side files...", jobs.len()));
    failed.extend(download_all(jobs, log));
    finish_downloads(failed)?;
    if c["noConfigs"] != true {
        push(log, "Downloading configs...");
        let zip = dir.join("_configs.zip");
        let base = format!("{}/packs/{}/versions/{}", sources::ATL_CDN, crate::enc(safe), crate::enc(&ver));
        crate::download(&format!("{base}/Configs.zip"), &zip)?;
        extract(&zip, dir, log)?;
        std::fs::remove_file(&zip).map_err(s)?;
    }
    Ok((flavor, mc, loader))
}

/// Where each server-side mod in an ATLauncher config comes from and goes to. Optional mods are
/// included when the pack recommends them, like ATLauncher's own install. Also returns the mods
/// whose CurseForge download couldn't be resolved.
fn atl_jobs(c: &Value, mc: &str, dir: &Path, log: &Log) -> Result<(Vec<crate::Job>, Vec<String>), String> {
    let (mut jobs, mut unresolved) = (vec![], vec![]);
    for m in c["mods"].as_array().into_iter().flatten() {
        let name = m["name"].as_str().unwrap_or("?");
        if m["server"] == false || (m["optional"] == true && m["recommended"] != true) {
            continue;
        }
        let sub = match m["type"].as_str().unwrap_or("mods") {
            "mods" => "mods".to_string(),
            t @ ("coremods" | "plugins") => t.to_string(),
            "dependency" => format!("mods/{mc}"),
            "resourcepack" | "texturepack" | "shaderpack" => continue,
            other => {
                push(log, format!("WARN skipped {name}: Octo can't install {other} files"));
                continue;
            }
        };
        let file = m["file"].as_str().unwrap_or(name);
        let ids = (m["curse_id"].as_u64(), m["curse_file_id"].as_u64());
        let url = match (m["download"].as_str(), m["url"].as_str(), ids) {
            (Some("server"), Some(u), _) => format!("{}/{}", sources::ATL_CDN, u.split('/').map(crate::enc).collect::<Vec<_>>().join("/")),
            (Some("direct"), Some(u), _) => sources::mediafilez(u),
            (_, _, (Some(p), Some(f))) => match sources::cf_download_url(p, f) {
                Ok(u) => u,
                Err(e) => {
                    push(log, format!("WARN {name}: {e}"));
                    unresolved.push(name.to_string());
                    continue;
                }
            },
            _ => {
                push(log, format!("WARN {name} has to be downloaded by hand: {}", m["website"].as_str().unwrap_or("")));
                continue;
            }
        };
        let to = safe_join(dir, &format!("{sub}/{file}")).ok_or(format!("modpack has an unsafe path: {file}"))?;
        jobs.push((url, to, None));
    }
    Ok((jobs, unresolved))
}

fn cf_manifest(dir: &Path) -> Option<Value> {
    let m: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).ok()?).ok()?;
    (m["manifestType"] == "minecraftModpack").then_some(m)
}

/// CurseForge client pack (no server pack offered): download every mod, apply overrides, then
/// move client-only mods aside.
fn cf_client_pack(dir: &Path, m: &Value, log: &Log) -> Result<Detected, String> {
    let mc = m["minecraft"]["version"].as_str().ok_or("manifest has no Minecraft version")?.to_string();
    let loaders = m["minecraft"]["modLoaders"].as_array().cloned().unwrap_or_default();
    let primary = loaders.iter().find(|l| l["primary"] == true).or(loaders.first());
    let (name, ver) = primary.and_then(|l| l["id"].as_str()?.split_once('-')).unwrap_or(("vanilla", ""));
    let flavor = flavor_from(name).ok_or(format!("{name} modpacks aren't supported"))?;
    let mods = dir.join("mods");
    let files = m["files"].as_array().cloned().unwrap_or_default();
    push(log, format!("Resolving {} mods on CurseForge...", files.len()));
    let (mut jobs, mut unresolved, mut ids) = (vec![], vec![], crate::worlds::CfIds::new());
    for f in &files {
        let (p, id) = (f["projectID"].as_u64().unwrap_or(0), f["fileID"].as_u64().unwrap_or(0));
        match sources::cf_download_url(p, id) {
            Ok(url) => {
                ids.insert(crate::url_file_name(&url), (p, id));
                jobs.push((url.clone(), mods.join(crate::url_file_name(&url)), None));
            }
            Err(e) => {
                push(log, format!("WARN {e}"));
                unresolved.push(format!("CurseForge file {id}"));
            }
        }
    }
    unresolved.extend(download_all(jobs, log));
    finish_downloads(unresolved)?;
    let overrides = m["overrides"].as_str().unwrap_or("overrides");
    if let Some(o) = safe_join(dir, overrides).filter(|o| o.is_dir()) {
        move_into(&o, dir)?;
    }
    let _ = std::fs::remove_file(dir.join("manifest.json"));
    let _ = std::fs::remove_file(dir.join("modlist.html"));
    if mods.is_dir() {
        crate::worlds::clean_mods(dir, &ids, log)?;
    }
    Ok((flavor, mc, (!ver.is_empty()).then(|| ver.to_string())))
}

fn subdirs(p: PathBuf) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// Best-effort detection for plain server-files zips.
pub fn detect(dir: &Path) -> Option<Detected> {
    // ServerPackCreator / ATM-style start scripts
    if let Ok(v) = std::fs::read_to_string(dir.join("variables.txt")) {
        let get = |k: &str| {
            v.lines().find_map(|l| l.trim().strip_prefix(k)?.trim_start().strip_prefix('=')).map(|x| x.trim().trim_matches('"').to_string())
        };
        if let (Some(mc), Some(f)) = (get("MINECRAFT_VERSION"), get("MODLOADER").and_then(|l| flavor_from(&l))) {
            return Some((f, mc, get("MODLOADER_VERSION").filter(|x| !x.is_empty())));
        }
    }
    // ATM / Create: Arcane Engineering style: FORGE_VERSION=40.2.9 ... INSTALLER="forge-1.18.2-$FORGE_VERSION-installer.jar"
    for name in subdirs(dir.to_path_buf()).iter().filter(|n| n.ends_with(".sh") || n.ends_with(".bat")) {
        let Ok(text) = std::fs::read_to_string(dir.join(name)) else { continue };
        let var = |k: &str| {
            text.lines().find_map(|l| {
                let l = l.trim();
                let l = ["set ", "SET ", "Set ", "export "].iter().find_map(|p| l.strip_prefix(p)).unwrap_or(l);
                l.strip_prefix(k)?.strip_prefix('=').map(|v| v.trim().trim_matches('"').to_string())
            })
        };
        if let Some(v) = var("NEOFORGE_VERSION") {
            return Some((Flavor::NeoForge, neo_mc(&v), Some(v)));
        }
        if let Some(v) = var("FORGE_VERSION") {
            let mc = var("MC_VERSION").or(var("MINECRAFT_VERSION")).or_else(|| {
                let after = text.split("forge-").nth(1)?;
                let mc: String = after.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
                (!mc.is_empty()).then_some(mc)
            });
            if let Some(mc) = mc {
                return Some((Flavor::Forge, mc, Some(v)));
            }
        }
    }
    let names = subdirs(dir.to_path_buf());
    for name in &names {
        if let Some(v) = name.strip_prefix("neoforge-").and_then(|n| n.strip_suffix("-installer.jar")) {
            return Some((Flavor::NeoForge, neo_mc(v), Some(v.into())));
        }
        if let Some((mc, v)) = name.strip_prefix("forge-").and_then(|n| n.strip_suffix("-installer.jar")?.split_once('-')) {
            return Some((Flavor::Forge, mc.into(), Some(v.into())));
        }
        // fabric-server-mc.1.21.1-loader.0.19.5-launcher.1.1.2.jar
        if let Some((mc, rest)) = name.strip_prefix("fabric-server-mc.").and_then(|n| n.split_once("-loader.")) {
            return Some((Flavor::Fabric, mc.into(), rest.split_once("-launcher").map(|(v, _)| v.into())));
        }
        // old Forge: forge-1.7.10-10.13.4.1614-1.7.10-universal.jar / forge-1.12.2-14.23.5.2860.jar
        if let Some((mc, rest)) = name.strip_prefix("forge-").and_then(|n| n.strip_suffix(".jar")?.split_once('-'))
            && !rest.contains("installer")
            && !rest.contains("shim")
        {
            let v = rest.trim_end_matches("-universal").trim_end_matches(&format!("-{mc}"));
            return Some((Flavor::Forge, mc.into(), Some(v.into())));
        }
        // paper-1.20.1-196.jar
        if let Some((mc, _)) = name.strip_prefix("paper-").and_then(|n| n.strip_suffix(".jar")?.rsplit_once('-')) {
            return Some((Flavor::Paper, mc.into(), None));
        }
    }
    // a bare vanilla jar is the weakest hint, so check it last
    if let Some(mc) = names.iter().find_map(|n| n.strip_prefix("minecraft_server.")?.strip_suffix(".jar")) {
        return Some((Flavor::Vanilla, mc.into(), None));
    }
    let lib = dir.join("libraries/net");
    if let Some(v) = subdirs(lib.join("neoforged/neoforge")).pop() {
        return Some((Flavor::NeoForge, neo_mc(&v), Some(v)));
    }
    if let Some((mc, v)) = subdirs(lib.join("minecraftforge/forge"))
        .pop()
        .and_then(|x| Some((x.split_once('-')?.0.to_string(), x.split_once('-')?.1.to_string())))
    {
        return Some((Flavor::Forge, mc, Some(v)));
    }
    if let Some(mc) = subdirs(lib.join("fabricmc/intermediary")).pop() {
        return Some((Flavor::Fabric, mc, subdirs(lib.join("fabricmc/fabric-loader")).pop()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_launch_prefers_native_script() {
        let d = std::env::temp_dir().join(format!("octo-ready-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("libraries")).unwrap();
        for f in ["libraries/unix_args.txt", "libraries/win_args.txt", "user_jvm_args.txt", "pack.jar"] {
            std::fs::write(d.join(f), "").unwrap();
        }
        std::fs::write(d.join("run.sh"), "#!/bin/sh\njava @user_jvm_args.txt @libraries/unix_args.txt \"$@\"\n").unwrap();
        std::fs::write(d.join("run.bat"), "@echo off\r\njava -Xmx4G @user_jvm_args.txt @libraries/win_args.txt %*\r\npause\r\n").unwrap();
        let args = ready_launch(&d).unwrap();
        let want = if cfg!(windows) { "@libraries/win_args.txt" } else { "@libraries/unix_args.txt" };
        assert_eq!(args, ["@user_jvm_args.txt", want, "nogui"]);
        // a line depending on unresolved variables is not "ready"
        std::fs::write(d.join("run.sh"), "java -jar $SERVER_JAR nogui\n").unwrap();
        std::fs::remove_file(d.join("run.bat")).unwrap();
        assert_eq!(ready_launch(&d), None);
        // quoted Windows java path with spaces + uppercase SET
        std::fs::write(
            d.join("start.bat"),
            "SET FORGE_VERSION=40.2.9\r\n\"C:\\Program Files\\Java\\bin\\java.exe\" -Xms2G -jar pack.jar\r\n",
        )
        .unwrap();
        assert_eq!(ready_launch(&d).unwrap(), ["-jar", "pack.jar", "nogui"]);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn folder_import_skips_wrapper_folders() {
        let d = std::env::temp_dir().join(format!("octo-wrap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let real = d.join("server").join("GT_New_Horizons_2.8.4_Server");
        std::fs::create_dir_all(real.join("mods")).unwrap();
        std::fs::write(real.join("startserver.bat"), "").unwrap();
        assert_eq!(server_root(&d.join("server")), real);
        // a pack whose only folder is mods/ is already the root
        let mods_only = d.join("modsonly");
        std::fs::create_dir_all(mods_only.join("mods")).unwrap();
        assert_eq!(server_root(&mods_only), mods_only);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn atl_picks_server_files() {
        let c: Value = serde_json::from_str(
            r#"{"mods":[
            {"name":"Create","file":"create.jar","type":"mods","download":"server","url":"packs/P/files/[1.21] create v1.jar","server":true},
            {"name":"Iris","file":"iris.jar","type":"mods","download":"server","url":"packs/P/files/iris.jar","client":true,"server":false},
            {"name":"Extra","file":"extra.jar","type":"mods","download":"direct","url":"https://edge.forgecdn.net/files/1/2/extra.jar","optional":true,"recommended":true},
            {"name":"Skipped","file":"skip.jar","type":"mods","download":"direct","url":"https://x/skip.jar","optional":true},
            {"name":"Lib","file":"lib.jar","type":"dependency","download":"direct","url":"https://x/lib.jar"},
            {"name":"Pack","file":"faithful.zip","type":"resourcepack","download":"direct","url":"https://x/f.zip"},
            {"name":"Manual","file":"m.jar","type":"mods","download":"browser","website":"https://example.com/m"}
        ]}"#,
        )
        .unwrap();
        let (d, log) = (Path::new("/srv/s"), Log::default());
        let (jobs, unresolved) = atl_jobs(&c, "1.12.2", d, &log).unwrap();
        assert_eq!(
            jobs,
            [
                (format!("{}/packs/P/files/%5B1.21%5D%20create%20v1.jar", sources::ATL_CDN), d.join("mods/create.jar"), None),
                ("https://mediafilez.forgecdn.net/files/1/2/extra.jar".into(), d.join("mods/extra.jar"), None),
                ("https://x/lib.jar".into(), d.join("mods/1.12.2/lib.jar"), None),
            ]
        );
        assert!(unresolved.is_empty());
        assert!(log.lock().unwrap().iter().any(|l| l.contains("Manual has to be downloaded by hand: https://example.com/m")));
    }

    /// cargo test live_technic_atl -- --ignored
    #[test]
    #[ignore]
    fn live_technic_atl() {
        assert!(sources::technic_search("tekkit").unwrap().iter().any(|h| h.slug.ends_with("/tekkit")));
        assert!(sources::technic_server_zip("tekkit").unwrap().ends_with(".zip"));
        let hits = sources::atl_search("all the forge").unwrap();
        let safe = hits.iter().find_map(|h| sources::parse_atl(&h.slug)).unwrap();
        let (_, c) = sources::atl_config(&safe).unwrap();
        let (jobs, _) = atl_jobs(&c, c["minecraft"].as_str().unwrap(), Path::new("/srv/s"), &Log::default()).unwrap();
        assert!(jobs.len() > 50, "{safe}: {} jobs", jobs.len());
        let probe = std::env::temp_dir().join(format!("octo-atl-{}.jar", std::process::id()));
        crate::download(&jobs[0].0, &probe).unwrap();
        assert!(std::fs::metadata(&probe).unwrap().len() > 0);
        std::fs::remove_file(probe).unwrap();
    }

    #[test]
    fn unsafe_paths_rejected() {
        let d = Path::new("/srv");
        assert!(safe_join(d, "mods/a.jar").is_some());
        assert!(safe_join(d, "../evil").is_none());
        assert!(safe_join(d, "/etc/passwd").is_none());
        assert!(safe_join(d, "mods/../../x").is_none());
    }

    #[test]
    fn detects_server_files() {
        let d = std::env::temp_dir().join(format!("octo-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("libraries/net/minecraftforge/forge/1.20.1-47.3.0")).unwrap();
        assert_eq!(detect(&d), Some((Flavor::Forge, "1.20.1".into(), Some("47.3.0".into()))));
        std::fs::write(d.join("variables.txt"), "MINECRAFT_VERSION=1.21.1\nMODLOADER=NeoForge\nMODLOADER_VERSION=21.1.80\n").unwrap();
        assert_eq!(detect(&d), Some((Flavor::NeoForge, "1.21.1".into(), Some("21.1.80".into()))));
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("fabric-server-mc.1.21.1-loader.0.19.5-launcher.1.1.2.jar"), "").unwrap();
        assert_eq!(detect(&d), Some((Flavor::Fabric, "1.21.1".into(), Some("0.19.5".into()))));
        std::fs::remove_dir_all(&d).unwrap();
    }
}

#[cfg(test)]
mod e2e {
    use super::*;

    /// A CurseForge pack without server files (Fabulously Optimized): refused from a download
    /// tab; from Import it needs the clean prompt, then comes out with client-only mods moved aside.
    /// cargo test cf_client_pack_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn cf_client_pack_live() {
        let d = std::env::temp_dir().join(format!("octo-cfclient-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // by id: cfwidget doesn't resolve this pack's slug
        let zip = d.join("fo.zip");
        crate::download(&sources::cf_download_url(396246, 8872574).unwrap(), &zip).unwrap();
        let src = zip.to_string_lossy().into_owned();
        let fresh = |n: &str| {
            let p = d.join(n);
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        assert_eq!(import(&src, &fresh("dl"), None, false, true, &Log::default()).unwrap_err(), NOT_SERVER_PACK);
        assert_eq!(import(&src, &fresh("ask"), None, false, false, &Log::default()).unwrap_err(), CLIENT_PACK);
        let (dir, log) = (fresh("clean"), Log::default());
        let r = import(&src, &dir, None, true, false, &log);
        log.lock().unwrap().iter().filter(|l| l.contains("client-only") || l.contains("WARN")).for_each(|l| println!("{l}"));
        assert_eq!(r.unwrap().0, Flavor::Fabric);
        assert!(std::fs::read_dir(dir.join(crate::worlds::CLIENT_ONLY)).unwrap().count() > 0, "client-only mods moved aside");
        assert!(!dir.join("_client_mods").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// E2E_DIR=/tmp/x cargo test e2e_import -- --ignored --nocapture
    #[test]
    #[ignore]
    fn e2e_import() {
        let root = PathBuf::from(std::env::var("E2E_DIR").unwrap());
        let local = std::env::var("E2E_LOCAL_ZIP").unwrap_or_default();
        let mut cases = vec![
            ("modrinth", "https://modrinth.com/modpack/adrenaserver"),
            ("cf_server", "https://www.curseforge.com/minecraft/modpacks/cobblemon-fabric"),
        ];
        if !local.is_empty() {
            cases = vec![("local", local.as_str())];
        }
        for (name, src) in cases {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let log = Log::default();
            let r = import(src, &dir, None, false, false, &log);
            let mods = std::fs::read_dir(dir.join("mods")).map(|d| d.count()).unwrap_or(0);
            println!("{name}: {r:?} mods={mods}");
            if let Ok((f, mc, l)) = &r {
                let java = crate::java::find(crate::flavors::java_major(mc).unwrap()).unwrap();
                let res = crate::flavors::install(*f, mc, l.as_deref(), &dir, &java, &log);
                println!("  loader install: {res:?} args={:?}", crate::flavors::launch_args(*f, &dir));
            }
            for l in log.lock().unwrap().iter().filter(|l| l.contains("WARN") || l.contains("Downloading") || l.contains("Note")) {
                println!("  {l}");
            }
        }
        // add-ons
        let log = Log::default();
        let fab = root.join("mods_fabric");
        println!("mr+deps: {:?}", sources::add_mod("cobblemon", "1.21.1", Flavor::Fabric, &fab, &log));
        println!(
            "cf mod:  {:?}",
            sources::add_mod("https://www.curseforge.com/minecraft/mc-mods/jei", "1.21.1", Flavor::Fabric, &fab, &log)
        );
        println!(
            "paper:   {:?}",
            sources::add_mod("https://modrinth.com/plugin/luckperms", "1.21.1", Flavor::Paper, &root.join("paper"), &log)
        );
        for l in log.lock().unwrap().iter() {
            println!("  {l}");
        }
    }
}

#[cfg(test)]
mod e2e_local {
    use super::*;

    /// Full GUI path for a downloaded server pack: create_with + import + start, wait for "Done".
    /// OCTO_DATA_DIR=/tmp/x E2E_PACK=/path/to/pack.zip cargo test e2e_local_pack -- --ignored --nocapture
    #[test]
    #[ignore]
    fn e2e_local_pack() {
        let src = std::env::var("E2E_PACK").unwrap();
        let log = Log::default();
        let r = crate::server::create_with("e2e", 4096, &log, |dir, log| import(&src, dir, None, false, false, log));
        for l in log.lock().unwrap().iter().filter(|l| !l.starts_with('[')) {
            println!("  {l}");
        }
        r.unwrap();
        let mut srv = crate::server::load_all().into_iter().find(|s| s.name == "e2e").unwrap();
        println!("config: {:?} {} java{} launch={:?}", srv.cfg.flavor, srv.cfg.mc_version, srv.cfg.java_major, srv.cfg.launch);
        srv.start(&crate::java::find(srv.cfg.java_major).unwrap()).unwrap();
        let t0 = std::time::Instant::now();
        let done = loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let found = srv.console.lock().unwrap().iter().find(|l| l.contains("Done (")).cloned();
            if found.is_some() {
                break found;
            }
            if !srv.running() || t0.elapsed().as_secs() > 600 {
                break None;
            }
        };
        let tail: Vec<String> = srv.console.lock().unwrap().iter().rev().take(8).cloned().collect();
        srv.stop();
        srv.wait_or_kill(std::time::Duration::from_secs(90));
        println!("result after {}s: {done:?}", t0.elapsed().as_secs());
        if done.is_none() {
            tail.iter().rev().for_each(|l| println!("  | {l}"));
        }
        assert!(done.is_some());
    }
}
