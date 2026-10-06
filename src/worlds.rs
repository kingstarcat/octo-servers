//! Find singleplayer worlds in the Minecraft launcher and modded launchers, and turn one
//! (plus the mods and configs of the instance it came from) into a dedicated server.
use crate::flavors::Flavor;
use crate::packs::Detected;
use crate::sources::flavor_from;
use crate::{Log, push, s};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct Found {
    pub name: String,
    /// "Minecraft" or "<launcher>: <instance>"
    pub place: String,
    pub dir: PathBuf,
    pub played: SystemTime,
}

/// Folders a launcher keeps its instances in. Each instance's game folder (the one with
/// saves and mods) is the instance itself or `.minecraft`, `minecraft` or `instance` inside it.
fn instance_roots() -> Vec<(&'static str, PathBuf)> {
    let (data, local, home, docs) = (dirs::data_dir(), dirs::data_local_dir(), dirs::home_dir(), dirs::document_dir());
    let at = |base: &Option<PathBuf>, p: &str| base.as_ref().map(|b| b.join(p));
    [
        ("Prism Launcher", at(&data, "PrismLauncher/instances")),
        ("Prism Launcher", at(&home, ".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances")),
        ("PolyMC", at(&data, "PolyMC/instances")),
        ("MultiMC", at(&data, "MultiMC/instances")),
        ("CurseForge", at(&home, "curseforge/minecraft/Instances")),
        ("CurseForge", at(&docs, "Curseforge/Minecraft/Instances")),
        ("Modrinth", at(&data, "ModrinthApp/profiles")),
        ("Modrinth", at(&data, "com.modrinth.theseus/profiles")),
        ("ATLauncher", at(&data, "ATLauncher/instances")),
        ("GDLauncher", at(&data, "gdlauncher_next/instances")),
        ("GDLauncher", at(&data, "gdlauncher_carbon/data/instances")),
        ("FTB App", at(&local, ".ftba/instances")),
        ("FTB App", at(&home, ".ftba/instances")),
        ("Technic", at(&data, ".technic/modpacks")),
        ("Technic", at(&home, ".technic/modpacks")),
    ]
    .into_iter()
    .filter_map(|(l, p)| Some((l, p?)))
    .collect()
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    v.sort();
    v
}

fn worlds_in(game: &Path, place: &str, out: &mut Vec<Found>) {
    for w in subdirs(&game.join("saves")) {
        let Ok(played) = w.join("level.dat").metadata().and_then(|m| m.modified()) else { continue };
        let name = w.file_name().unwrap_or_default().to_string_lossy().into_owned();
        out.push(Found { name, place: place.into(), dir: w, played });
    }
}

/// Every world on this PC we know where to look for, most recently played first.
pub fn scan() -> Vec<Found> {
    let mut out = vec![];
    // the official launcher, and launchers that share its folder (TLauncher, Lunar, Feather, Badlion)
    let mut games: Vec<PathBuf> = [dirs::data_dir(), dirs::home_dir()].into_iter().flatten().map(|d| d.join(".minecraft")).collect();
    games.dedup();
    for g in games {
        worlds_in(&g, "Minecraft", &mut out);
    }
    for (launcher, root) in instance_roots() {
        for i in subdirs(&root) {
            let name = i.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if let Some(g) = [".minecraft", "minecraft", "instance", ""].iter().map(|g| i.join(g)).find(|g| g.join("saves").is_dir()) {
                worlds_in(&g, &format!("{launcher}: {name}"), &mut out);
            }
        }
    }
    out.sort_by_key(|w| std::cmp::Reverse(w.played));
    out
}

/// A world in `<game>/saves/<world>` belongs to `<game>`, which holds the instance's mods and configs.
fn game_dir(world: &Path) -> Option<&Path> {
    let saves = world.parent()?;
    (saves.file_name()? == "saves").then(|| saves.parent()).flatten()
}

/// "1.20.1" from a level.dat (gzipped NBT): the string `Name` inside the `Version` compound.
/// Worlds older than 1.9 don't have it.
pub fn level_version(level_dat: &Path) -> Option<String> {
    use std::io::Read;
    let mut nbt = vec![];
    flate2::read::GzDecoder::new(std::fs::File::open(level_dat).ok()?).read_to_end(&mut nbt).ok()?;
    // ponytail: byte search for the two tags instead of an NBT parser; parse properly if this ever misreads.
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let v = find(&nbt, b"\x0a\x00\x07Version")?;
    let rest = &nbt[v..nbt.len().min(v + 256)];
    let n = find(rest, b"\x08\x00\x04Name")? + 7;
    let len = u16::from_be_bytes([*rest.get(n)?, *rest.get(n + 1)?]) as usize;
    String::from_utf8(rest.get(n + 2..n + 2 + len)?.to_vec()).ok()
}

fn json(p: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()
}

fn loader(name: &str, mc: String, ver: Option<&str>) -> Result<Option<Detected>, String> {
    let name = name.to_ascii_lowercase();
    if name.contains("quilt") {
        return Err("This world is from a Quilt instance, and Quilt servers aren't supported.".into());
    }
    let f = if name.contains("neoforge") || name == "net.neoforged" {
        Flavor::NeoForge
    } else if name.contains("forge") {
        Flavor::Forge
    } else if name.contains("fabric") {
        Flavor::Fabric
    } else {
        return Ok(flavor_from(&name).map(|f| (f, mc, None)));
    };
    let ver = ver.map(|v| v.trim_start_matches(&format!("{mc}-")).to_string()).filter(|v| !v.is_empty());
    Ok(Some((f, mc, ver)))
}

/// Loader and Minecraft version from the launcher's instance file, looked for in the game
/// folder and the instance folder above it.
fn from_instance(game: &Path) -> Result<Option<Detected>, String> {
    for d in [Some(game), game.parent()].into_iter().flatten() {
        // Prism / MultiMC / PolyMC
        if let Some(m) = json(&d.join("mmc-pack.json")) {
            let comps = m["components"].as_array().cloned().unwrap_or_default();
            let ver = |uid: &str| comps.iter().find(|c| c["uid"] == uid).and_then(|c| c["version"].as_str().map(String::from));
            let Some(mc) = ver("net.minecraft") else { continue };
            for uid in ["org.quiltmc.quilt-loader", "net.neoforged", "net.minecraftforge", "net.fabricmc.fabric-loader"] {
                if let Some(v) = ver(uid) {
                    return loader(uid, mc, Some(&v));
                }
            }
            return Ok(Some((Flavor::Vanilla, mc, None)));
        }
        // CurseForge: "forge-47.2.0", "neoforge-21.1.77", "fabric-0.15.11-1.20.1"
        if let Some(m) = json(&d.join("minecraftinstance.json"))
            && let Some(mc) = m["gameVersion"].as_str()
        {
            return match m["baseModLoader"]["name"].as_str().and_then(|n| n.split_once('-')) {
                Some((name, v)) => loader(name, mc.into(), v.split('-').next()),
                None => Ok(Some((Flavor::Vanilla, mc.into(), None))),
            };
        }
        // ATLauncher
        if let Some(m) = json(&d.join("instance.json"))
            && let Some(mc) = m["id"].as_str().filter(|v| v.starts_with("1."))
        {
            let l = &m["launcher"]["loaderVersion"];
            return match l["type"].as_str() {
                Some(t) => loader(t, mc.into(), l["version"].as_str()),
                None => Ok(Some((Flavor::Vanilla, mc.into(), None))),
            };
        }
        // Modrinth App (older versions), GDLauncher
        for (file, mc, name, ver) in [
            ("profile.json", "/metadata/game_version", "/metadata/loader", "/metadata/loader_version/id"),
            ("config.json", "/loader/mcVersion", "/loader/loaderType", "/loader/loaderVersion"),
        ] {
            if let Some(m) = json(&d.join(file))
                && let Some(mc) = m.pointer(mc).and_then(Value::as_str)
            {
                let name = m.pointer(name).and_then(Value::as_str).unwrap_or("vanilla");
                return loader(name, mc.into(), m.pointer(ver).and_then(Value::as_str));
            }
        }
    }
    Ok(None)
}

/// No instance file: guess the loader from what the mod jars declare.
fn from_mods(mods: &Path) -> Option<Flavor> {
    let (mut fabric, mut forge, mut neo) = (0, 0, 0);
    for jar in jars(mods) {
        let Ok(mut z) = std::fs::File::open(&jar).map_err(s).and_then(|f| zip::ZipArchive::new(f).map_err(s)) else { continue };
        if z.by_name("META-INF/neoforge.mods.toml").is_ok() {
            neo += 1;
        } else if z.by_name("META-INF/mods.toml").is_ok() || z.by_name("mcmod.info").is_ok() {
            forge += 1;
        } else if z.by_name("fabric.mod.json").is_ok() {
            fabric += 1;
        }
    }
    let best = [(neo, Flavor::NeoForge), (forge, Flavor::Forge), (fabric, Flavor::Fabric)].into_iter().max_by_key(|(n, _)| *n)?;
    (best.0 > 0).then_some(best.1)
}

fn jars(mods: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(mods)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jar"))
        .collect();
    v.sort();
    v
}

/// Mods that declare themselves client-only in their own metadata.
fn says_client_only(jar: &Path) -> bool {
    use std::io::Read;
    let Ok(mut z) = std::fs::File::open(jar).map_err(s).and_then(|f| zip::ZipArchive::new(f).map_err(s)) else { return false };
    let mut text = |name: &str| {
        let mut t = String::new();
        z.by_name(name).ok().and_then(|mut f| f.read_to_string(&mut t).ok()).map(|_| t)
    };
    if let Some(j) = text("fabric.mod.json").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        return j["environment"] == "client";
    }
    ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"]
        .iter()
        .filter_map(|n| text(n))
        .any(|t| t.lines().any(|l| l.split('#').next().unwrap_or("").replace(' ', "") == "clientSideOnly=true"))
}

/// Blocks, recipes, loot or worldgen: the world depends on it, so never leave it out on
/// Modrinth's word alone (its pams-harvestcraft page says server "unsupported").
fn adds_content(jar: &Path) -> bool {
    let Ok(z) = std::fs::File::open(jar).map_err(s).and_then(|f| zip::ZipArchive::new(f).map_err(s)) else { return true };
    z.file_names().any(|n| {
        (n.starts_with("assets/") && n.contains("/blockstates/"))
            || (n.starts_with("data/") && ["/recipe", "/loot_table", "/worldgen/"].iter().any(|d| n.contains(d)))
    })
}

/// CurseForge (project, file) ids by jar name, from what the launcher recorded: Prism's
/// `mods/.index/*.pw.toml`, the CurseForge app's `minecraftinstance.json`, ATLauncher's
/// `instance.json` and GDLauncher's `config.json`.
fn cf_ids(mods: &Path) -> std::collections::HashMap<String, (u64, u64)> {
    let mut ids = std::collections::HashMap::new();
    for f in std::fs::read_dir(mods.join(".index")).into_iter().flatten().flatten() {
        let Ok(text) = std::fs::read_to_string(f.path()) else { continue };
        let val = |k: &str| {
            text.lines()
                .find_map(|l| l.trim().strip_prefix(k)?.trim_start().strip_prefix('='))
                .map(|v| v.trim().trim_matches(['\'', '"']).to_string())
        };
        if let (Some(name), Some(p), Some(id)) = (val("filename"), val("project-id"), val("file-id"))
            && let (Ok(p), Ok(id)) = (p.parse(), id.parse())
        {
            ids.insert(name, (p, id));
        }
    }
    // (file, mod list, jar name, project id, file id)
    for (file, list, name, p, id) in [
        ("minecraftinstance.json", "/installedAddons", "/installedFile/fileName", "/addonID", "/installedFile/id"),
        ("instance.json", "/launcher/mods", "/file", "/curseForgeProjectId", "/curseForgeFileId"), // ATLauncher
        ("config.json", "/mods", "/fileName", "/projectID", "/fileID"),                            // GDLauncher
    ] {
        let Some(m) = mods.parent().and_then(|g| json(&g.join(file))) else { continue };
        for a in m.pointer(list).and_then(Value::as_array).into_iter().flatten() {
            let get = |k: &str| a.pointer(k);
            if let (Some(n), Some(p), Some(id)) =
                (get(name).and_then(Value::as_str), get(p).and_then(Value::as_u64), get(id).and_then(Value::as_u64))
            {
                ids.insert(n.to_string(), (p, id));
            }
        }
    }
    ids
}

/// Copy the instance's mods into `dir/mods`, leaving out client-only ones (their own metadata,
/// then Modrinth's server_side for the rest). Returns what was left out.
fn copy_mods(from: &Path, dir: &Path, log: &Log) -> Result<Vec<String>, String> {
    let all = jars(from);
    let mut client: Vec<PathBuf> = all.iter().filter(|j| says_client_only(j)).cloned().collect();
    let rest: Vec<(String, &PathBuf)> =
        all.iter().filter(|j| !client.contains(j)).filter_map(|j| Some((crate::sources::sha1_file(j)?, j))).collect();
    push(log, format!("Checking {} mods on Modrinth for client-only ones...", rest.len()));
    match crate::sources::client_only_hashes(&rest.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>()) {
        Ok(hs) => {
            for (_, j) in rest.iter().filter(|(h, _)| hs.contains(h)) {
                if adds_content(j) {
                    let name = j.file_name().unwrap_or_default().to_string_lossy();
                    push(log, format!("Kept {name}: Modrinth lists it as client-only, but it adds blocks or recipes the world may use"));
                } else {
                    client.push((*j).clone());
                }
            }
        }
        Err(e) => push(log, format!("WARN couldn't reach Modrinth ({e}), so only mods that say so themselves are left out")),
    }
    let ids = cf_ids(from);
    let rest: Vec<(&PathBuf, (u64, u64))> = all
        .iter()
        .filter(|j| !client.contains(j))
        .filter_map(|j| Some((j, *ids.get(&j.file_name()?.to_string_lossy().into_owned())?)))
        .collect();
    if !rest.is_empty() {
        push(log, format!("Checking {} mods on CurseForge for client-only ones...", rest.len()));
        let tagged: Vec<&PathBuf> = std::thread::scope(|sc| {
            let jobs: Vec<_> = rest
                .chunks(rest.len().div_ceil(8))
                .map(|c| {
                    sc.spawn(move || {
                        c.iter().filter(|(_, (p, f))| crate::sources::cf_client_only(*p, *f)).map(|(j, _)| *j).collect::<Vec<_>>()
                    })
                })
                .collect();
            jobs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        for j in tagged {
            if adds_content(j) {
                let name = j.file_name().unwrap_or_default().to_string_lossy();
                push(log, format!("Kept {name}: CurseForge lists it as client-only, but it adds blocks or recipes the world may use"));
            } else {
                client.push(j.clone());
            }
        }
    }
    let to = dir.join("mods");
    std::fs::create_dir_all(&to).map_err(s)?;
    let aside = dir.join(CLIENT_ONLY);
    for j in &all {
        let dest = if client.contains(j) { &aside } else { &to };
        std::fs::create_dir_all(dest).map_err(s)?;
        std::fs::copy(j, dest.join(j.file_name().unwrap_or_default())).map_err(|e| format!("{}: {e}", j.display()))?;
    }
    push(log, format!("Copied {} mods", all.len() - client.len()));
    Ok(client.iter().map(|j| j.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect())
}

/// Where left-out mods go, so they can be put back by hand or by `test_start`.
pub const CLIENT_ONLY: &str = "mods-client-only";

/// Mod ids a jar declares (Forge / NeoForge mods.toml, Fabric, old Forge mcmod.info).
fn mod_ids(jar: &Path) -> Vec<String> {
    use std::io::Read;
    let Ok(mut z) = std::fs::File::open(jar).map_err(s).and_then(|f| zip::ZipArchive::new(f).map_err(s)) else { return vec![] };
    let mut text = |name: &str| {
        let mut t = String::new();
        z.by_name(name).ok().and_then(|mut f| f.read_to_string(&mut t).ok()).map(|_| t)
    };
    let mut ids: Vec<String> = ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"]
        .iter()
        .filter_map(|n| text(n))
        .flat_map(|t| {
            t.lines()
                .filter_map(|l| {
                    l.trim()
                        .strip_prefix("modId")?
                        .trim_start()
                        .strip_prefix('=')
                        .map(|v| v.split('#').next().unwrap_or("").trim().trim_matches(['"', '\'']).to_string())
                })
                .collect::<Vec<_>>()
        })
        .collect();
    for (file, key) in [("fabric.mod.json", "id"), ("mcmod.info", "modid")] {
        if let Some(v) = text(file).and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            let list = if v.is_array() { v.as_array().cloned().unwrap_or_default() } else { vec![v] };
            ids.extend(list.iter().filter_map(|m| m[key].as_str().map(String::from)));
        }
    }
    ids.retain(|i| !i.is_empty());
    ids
}

fn mentions(text: &str, id: &str) -> bool {
    let is_id = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    text.match_indices(id).any(|(i, _)| !text[..i].ends_with(is_id) && !text[i + id.len()..].starts_with(is_id))
}

/// Which jar in `mods` a client-side crash came from: mod ids the loader names ("ModID: x",
/// "provided by 'x'", "from mod x", "TRANSFORMER/x@1.0/" stack frames), then the first
/// non-Minecraft class in the stack trace, looked up in the jars.
fn client_crash_culprit(lines: &[String], mods: &Path) -> Option<PathBuf> {
    let text = lines.join("\n");
    // Minecraft's client classes, or the graphics libraries only the client has (Sodium's
    // pre-launch check dies on org/lwjgl/Version before any mod loads)
    let client = [
        "invalid dist",
        "net/minecraft/client/",
        "net.minecraft.client.",
        "environment type SERVER",
        "org/lwjgl/",
        "org.lwjgl.",
        "blaze3d",
    ];
    if !client.iter().any(|c| text.contains(c)) {
        return None;
    }
    let jars: Vec<(PathBuf, Vec<String>)> = jars(mods).into_iter().map(|j| (j.clone(), mod_ids(&j))).collect();
    let word = |rest: &str| rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect::<String>();
    let mut ids = vec![];
    for l in lines {
        for pat in ["ModID: ", "provided by '", "from mod ", "TRANSFORMER/"] {
            ids.extend(l.match_indices(pat).map(|(i, _)| word(&l[i + pat.len()..])));
        }
    }
    let skip = ["minecraft", "neoforge", "forge", "fml", "fabricloader", "mixinextras"];
    if let Some((j, _)) = ids.iter().filter(|i| !skip.contains(&i.as_str())).find_map(|id| jars.iter().find(|(_, ids)| ids.contains(id))) {
        return Some(j.clone());
    }
    // "at TRANSFORMER/x@1/com.foo.Bar.baz(Bar.java:1)" or "at com.foo.Bar.baz(Bar.java:1)"
    let vanilla = [
        "net.minecraft.",
        "net.neoforged.",
        "net.minecraftforge.",
        "net.fabricmc.",
        "cpw.",
        "com.mojang.",
        "java.",
        "jdk.",
        "sun.",
        "org.spongepowered.",
    ];
    for l in lines {
        let Some(frame) = l.trim().strip_prefix("at ") else { continue };
        let frame = frame.rsplit_once('/').map_or(frame, |(_, f)| f);
        let Some(class) = frame.split('(').next().and_then(|m| m.rsplit_once('.')).map(|(c, _)| c.split('$').next().unwrap_or(c)) else {
            continue;
        };
        if vanilla.iter().any(|v| class.starts_with(v)) {
            continue;
        }
        let entry = format!("{}.class", class.replace('.', "/"));
        let has = |j: &Path| {
            std::fs::File::open(j).ok().and_then(|f| zip::ZipArchive::new(f).ok()).is_some_and(|mut z| z.by_name(&entry).is_ok())
        };
        if let Some((j, _)) = jars.iter().find(|(j, _)| has(j)) {
            return Some(j.clone());
        }
    }
    None
}

/// Left-out mods a crash says are missing ("requires athena", "Mod ID: 'athena', Requested by").
fn missing_deps(lines: &[String], aside: &Path) -> Vec<PathBuf> {
    let text = lines.join("\n");
    if !["requires", "Requested by", "dependenc"].iter().any(|k| text.contains(k)) {
        return vec![];
    }
    jars(aside).into_iter().filter(|j| mod_ids(j).iter().any(|id| mentions(&text, id))).collect()
}

/// Start the new server once to check its mods load. A mod that crashes it with client-only
/// code is moved to `mods-client-only`; a left-out mod another one needs is put back. Up to a
/// few tries; it ends stopped either way. Runs on a free port so it can't clash with a running server.
pub fn test_start(dir: &Path, log: &Log) -> Result<(), String> {
    let (mods, aside) = (dir.join("mods"), dir.join(CLIENT_ONLY));
    if jars(&mods).is_empty() {
        return Ok(());
    }
    let props_path = dir.join("server.properties");
    let port = std::net::TcpListener::bind("0.0.0.0:0").and_then(|l| l.local_addr()).map_err(s)?.port().to_string();
    let original = std::fs::read_to_string(&props_path).ok().and_then(|t| crate::dashboard::props_get(&t, "server-port"));
    let set_port = |p: &str| {
        let text = std::fs::read_to_string(&props_path).unwrap_or_default();
        std::fs::write(&props_path, crate::dashboard::props_set(&text, &[("server-port", p)])).map_err(s)
    };
    let mut srv = crate::server::load(dir.to_path_buf()).ok_or("The new server's settings are missing")?;
    let java = crate::java::ensure(srv.cfg.java_major, log)?;
    let result = (|| {
        for attempt in 1..=6 {
            set_port(&port)?;
            push(log, format!("Test-starting the server to check the mods load, try {attempt}. Big packs take a few minutes."));
            srv.console.lock().unwrap().clear();
            srv.start(&java)?;
            let end = std::time::Instant::now() + std::time::Duration::from_secs(20 * 60);
            let done = |srv: &crate::server::Server| srv.console.lock().unwrap().iter().any(|l| l.contains("Done ("));
            while srv.running() && !done(&srv) && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if done(&srv) {
                srv.stop();
                srv.wait_or_kill(std::time::Duration::from_secs(120));
                push(log, "The server started fine with these mods. It's stopped now and ready to use.");
                return Ok(());
            }
            if srv.running() {
                srv.stop();
                srv.wait_or_kill(std::time::Duration::from_secs(60));
                return Err("The test start took over 20 minutes without finishing, so it was stopped. Start the server from its dashboard and watch the console.".into());
            }
            std::thread::sleep(std::time::Duration::from_secs(1)); // let the pipe threads catch the last lines
            let lines = srv.console.lock().unwrap().clone();
            let back = missing_deps(&lines, &aside);
            if !back.is_empty() {
                for j in &back {
                    let name = j.file_name().unwrap_or_default();
                    push(log, format!("Putting back {}: another mod needs it", name.to_string_lossy()));
                    std::fs::rename(j, mods.join(name)).map_err(s)?;
                }
                continue;
            }
            if let Some(j) = client_crash_culprit(&lines, &mods) {
                let name = j.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if adds_content(&j) {
                    return Err(format!(
                        "{name} crashes the server with client-only code, but it adds blocks or items the world may use, so it was left in. \
                         Check for a newer version of it, or remove it from the mods folder if you don't need it."
                    ));
                }
                push(log, format!("Moving {name} to {CLIENT_ONLY}: it crashed the server with client-only code"));
                std::fs::create_dir_all(&aside).map_err(s)?;
                std::fs::rename(&j, aside.join(&name)).map_err(s)?;
                continue;
            }
            return Err(format!("The test start crashed. {}", srv.crash.clone().unwrap_or_default()));
        }
        Err("The server still crashes after several fixes. The console on its dashboard shows the last error.".into())
    })();
    set_port(original.as_deref().unwrap_or("25565"))?;
    result
}

/// Fill a new server folder from a world: the world itself, and with `with_mods`, when it comes
/// from a modded instance, that instance's loader, mods (minus client-only ones) and configs.
/// Without, it's a vanilla server of the same Minecraft version.
pub fn import(src: &Path, dir: &Path, with_mods: bool, log: &Log) -> Result<Detected, String> {
    crate::server::import_world(dir, src, log)?;
    let game = src.is_dir().then(|| game_dir(src)).flatten();
    let level_mc = level_version(&dir.join("world").join("level.dat"));
    let found = match game {
        Some(g) => from_instance(g)?,
        None => None,
    };
    let mods = game.map(|g| g.join("mods")).filter(|m| with_mods && !jars(m).is_empty());
    let (flavor, mc, loader) = match found {
        Some((_, mc, _)) if !with_mods => (Flavor::Vanilla, mc, None),
        Some(d) => d,
        None => {
            let mc = level_mc.clone().ok_or(
                "Couldn't tell which Minecraft version this world is from (worlds older than 1.9 don't say). \
                 Create a blank server with the right version and use Import world on its dashboard instead.",
            )?;
            (mods.as_deref().and_then(from_mods).unwrap_or(Flavor::Vanilla), mc, None)
        }
    };
    if let Some(l) = level_mc.filter(|l| *l != mc) {
        push(log, format!("WARN the world was last played on Minecraft {l}, but its instance is set to {mc}. Using {mc}."));
    }
    if let (Some(g), Some(m), true) = (game, &mods, flavor != Flavor::Vanilla) {
        copy_instance(g, m, dir, log)?;
    }
    Ok((flavor, mc, loader))
}

/// Mods (minus client-only ones) and configs from instance game folder `g` into server `dir`.
fn copy_instance(g: &Path, mods: &Path, dir: &Path, log: &Log) -> Result<(), String> {
    let left_out = copy_mods(mods, dir, log)?;
    if !left_out.is_empty() {
        push(log, format!("Moved {} client-only mods to {CLIENT_ONLY}: {}", left_out.len(), left_out.join(", ")));
    }
    for extra in ["config", "defaultconfigs", "kubejs", "scripts"] {
        if g.join(extra).is_dir() {
            std::fs::create_dir_all(dir.join(extra)).map_err(s)?;
            let n = crate::packs::copy_dir(&g.join(extra), &dir.join(extra))?;
            push(log, format!("Copied {extra} ({n} files)"));
        }
    }
    Ok(())
}

/// Turn a client launcher instance unpacked into `dir` (Prism / MultiMC / ATLauncher export)
/// into server files: the instance's loader, its mods minus client-only ones, and its configs.
pub fn clean_instance(dir: &Path, log: &Log) -> Result<Detected, String> {
    let src = dir.join("_client");
    std::fs::create_dir_all(&src).map_err(s)?;
    for e in std::fs::read_dir(dir).map_err(s)?.flatten().filter(|e| e.file_name() != "_client") {
        std::fs::rename(e.path(), src.join(e.file_name())).map_err(s)?;
    }
    let game = [".minecraft", "minecraft"].map(|n| src.join(n)).into_iter().find(|p| p.is_dir()).unwrap_or(src.clone());
    let mods = game.join("mods");
    let detected = match from_instance(&game)? {
        Some(d) => d,
        None => {
            return Err("Couldn't tell which loader and Minecraft version this pack uses. Ask for the pack's server files instead.".into());
        }
    };
    copy_instance(&game, &mods, dir, log)?;
    std::fs::remove_dir_all(&src).map_err(s)?;
    Ok(detected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn level_dat(p: &Path, version: &str) {
        let mut nbt = b"\x0a\x00\x00\x0a\x00\x04Data\x03\x00\x0bDataVersion\x00\x00\x0d\x05".to_vec();
        nbt.extend(b"\x0a\x00\x07Version\x03\x00\x02Id\x00\x00\x0d\x05\x08\x00\x04Name");
        nbt.extend((version.len() as u16).to_be_bytes());
        nbt.extend(version.as_bytes());
        nbt.extend(b"\x00\x00\x00");
        let mut gz = flate2::write::GzEncoder::new(std::fs::File::create(p).unwrap(), flate2::Compression::default());
        gz.write_all(&nbt).unwrap();
        gz.finish().unwrap();
    }

    fn jar(p: &Path, file: &str, text: &str) {
        let mut z = zip::ZipWriter::new(std::fs::File::create(p).unwrap());
        z.start_file(file, zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(text.as_bytes()).unwrap();
        z.finish().unwrap();
    }

    #[test]
    fn reads_version_from_level_dat() {
        let d = std::env::temp_dir().join(format!("octo-level-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        level_dat(&d.join("level.dat"), "1.20.1");
        assert_eq!(level_version(&d.join("level.dat")).as_deref(), Some("1.20.1"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reads_launcher_instances() {
        let d = std::env::temp_dir().join(format!("octo-inst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let cases: [(&str, &str, &str, Option<Detected>); 5] = [
            (
                "prism/.minecraft",
                "../mmc-pack.json",
                r#"{"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.minecraftforge","version":"47.2.0"}]}"#,
                Some((Flavor::Forge, "1.20.1".into(), Some("47.2.0".into()))),
            ),
            (
                "cf",
                "minecraftinstance.json",
                r#"{"gameVersion":"1.20.1","baseModLoader":{"name":"fabric-0.15.11-1.20.1"}}"#,
                Some((Flavor::Fabric, "1.20.1".into(), Some("0.15.11".into()))),
            ),
            (
                "atl",
                "instance.json",
                r#"{"id":"1.21.1","launcher":{"loaderVersion":{"type":"NeoForge","version":"21.1.77"}}}"#,
                Some((Flavor::NeoForge, "1.21.1".into(), Some("21.1.77".into()))),
            ),
            (
                "gdl",
                "config.json",
                r#"{"loader":{"loaderType":"forge","mcVersion":"1.18.2","loaderVersion":"1.18.2-40.2.9"}}"#,
                Some((Flavor::Forge, "1.18.2".into(), Some("40.2.9".into()))),
            ),
            ("plain", "nothing.json", "{}", None),
        ];
        for (game, file, text, want) in cases {
            let g = d.join(game);
            std::fs::create_dir_all(&g).unwrap();
            std::fs::write(g.join(file), text).unwrap();
            assert_eq!(from_instance(&g).unwrap(), want, "{game}");
        }
        let q = d.join("quilt");
        std::fs::create_dir_all(&q).unwrap();
        std::fs::write(
            q.join("mmc-pack.json"),
            r#"{"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"org.quiltmc.quilt-loader","version":"0.26"}]}"#,
        )
        .unwrap();
        assert!(from_instance(&q).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn spots_client_only_mods_and_loader() {
        let d = std::env::temp_dir().join(format!("octo-mods-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        jar(&d.join("minimap.jar"), "fabric.mod.json", r#"{"id":"minimap","environment":"client"}"#);
        jar(&d.join("lithium.jar"), "fabric.mod.json", r#"{"id":"lithium","environment":"*"}"#);
        jar(&d.join("zoom.jar"), "META-INF/mods.toml", "modLoader=\"javafml\"\nclientSideOnly = true # zoom\n");
        jar(&d.join("create.jar"), "META-INF/mods.toml", "modLoader=\"javafml\"\n# clientSideOnly=true\n");
        assert!(says_client_only(&d.join("minimap.jar")));
        assert!(says_client_only(&d.join("zoom.jar")));
        assert!(!says_client_only(&d.join("lithium.jar")));
        assert!(!says_client_only(&d.join("create.jar")));
        assert!(!adds_content(&d.join("minimap.jar")));
        jar(&d.join("crops.jar"), "assets/crops/blockstates/corn.json", "{}");
        jar(&d.join("jeiaddon.jar"), "data/jeiaddon/recipe/x.json", "{}");
        assert!(adds_content(&d.join("crops.jar")) && adds_content(&d.join("jeiaddon.jar")));
        jar(&d.join("sodium.jar"), "fabric.mod.json", "{}");
        assert_eq!(from_mods(&d), Some(Flavor::Fabric));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reads_curseforge_ids() {
        let d = std::env::temp_dir().join(format!("octo-cfids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("mods/.index")).unwrap();
        std::fs::write(
            d.join("mods/.index/biome-music.pw.toml"),
            "filename = 'biomemusic-1.21.1-4.1.jar'\nside = 'server'\n\n[update.curseforge]\nfile-id = 6012345\nproject-id = 401234\n",
        )
        .unwrap();
        std::fs::write(
            d.join("minecraftinstance.json"),
            r#"{"installedAddons":[{"addonID":238222,"installedFile":{"id":5101366,"fileName":"jei.jar"}}]}"#,
        )
        .unwrap();
        std::fs::write(
            d.join("instance.json"),
            r#"{"launcher":{"mods":[{"file":"create.jar","curseForgeProjectId":328085,"curseForgeFileId":5838779},{"file":"local.jar"}]}}"#,
        )
        .unwrap();
        std::fs::write(d.join("config.json"), r#"{"mods":[{"fileName":"sodium.jar","projectID":394468,"fileID":5217345}]}"#).unwrap();
        let ids = cf_ids(&d.join("mods"));
        assert_eq!(ids.get("biomemusic-1.21.1-4.1.jar"), Some(&(401234, 6012345)));
        assert_eq!(ids.get("jei.jar"), Some(&(238222, 5101366)));
        assert_eq!(ids.get("create.jar"), Some(&(328085, 5838779)));
        assert_eq!(ids.get("sodium.jar"), Some(&(394468, 5217345)));
        assert_eq!(ids.get("local.jar"), None);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn finds_what_broke_the_test_start() {
        let d = std::env::temp_dir().join(format!("octo-culprit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (mods, aside) = (d.join("mods"), d.join(CLIENT_ONLY));
        std::fs::create_dir_all(&mods).unwrap();
        std::fs::create_dir_all(&aside).unwrap();
        jar(&mods.join("create.jar"), "META-INF/mods.toml", "[[mods]]\nmodId=\"create\"\n");
        jar(&mods.join("trepu.jar"), "META-INF/neoforge.mods.toml", "[[mods]]\nmodId = 'tensura_trepu' # ui\n");
        jar(&mods.join("hud.jar"), "com/hud/client/Overlay.class", "");
        jar(&mods.join("sodium.jar"), "net/caffeinemc/mods/sodium/client/compatibility/checks/PreLaunchChecks.class", "");
        jar(&aside.join("athena.jar"), "META-INF/mods.toml", "modId=\"athena\"\n");
        jar(&aside.join("iris.jar"), "fabric.mod.json", r#"{"id":"iris"}"#);
        let l = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
        let neo = l(
            "java.lang.RuntimeException: Attempted to load class net/minecraft/client/gui/screens/Screen for invalid dist DEDICATED_SERVER\n\
            \tat MC-BOOTSTRAP/net.neoforged.fancymodloader/net.neoforged.fml.X.y(X.java:1)\n\
            \tat TRANSFORMER/tensura_trepu@1.0.0.2/com.tensura_trepu.Main.<init>(Main.java:20)",
        );
        assert_eq!(client_crash_culprit(&neo, &mods), Some(mods.join("trepu.jar")));
        let forge = l("Failed to create mod instance. ModID: tensura_trepu, class com.tensura_trepu.Main\nnet/minecraft/client/Minecraft");
        assert_eq!(client_crash_culprit(&forge, &mods), Some(mods.join("trepu.jar")));
        let by_class =
            l("java.lang.NoClassDefFoundError: net/minecraft/client/gui/Gui\n\tat com.hud.client.Overlay$1.render(Overlay.java:5)");
        assert_eq!(client_crash_culprit(&by_class, &mods), Some(mods.join("hud.jar")));
        // the real one: Sodium on a NeoForge 1.21.1 server, before any mod loads
        let sodium = l("Exception in thread \"main\" java.lang.NoClassDefFoundError: org/lwjgl/Version\n\
            \tat LAYER SERVICE/sodium_service@0.8.13+mc1.21.1/net.caffeinemc.mods.sodium.client.compatibility.checks.PreLaunchChecks.isUsingKnownCompatibleLwjglVersion(PreLaunchChecks.java:136)\n\
            \tat MC-BOOTSTRAP/fml_loader@4.0.44/net.neoforged.fml.loading.ImmediateWindowHandler.load(ImmediateWindowHandler.java:45)");
        assert_eq!(client_crash_culprit(&sodium, &mods), Some(mods.join("sodium.jar")));
        assert_eq!(client_crash_culprit(&l("java.lang.OutOfMemoryError: Java heap space"), &mods), None, "not a client crash");

        let forge_dep =
            l("Missing or unsupported mandatory dependencies:\n\tMod ID: 'athena', Requested by: 'create', Expected range: '[4,)'");
        assert_eq!(missing_deps(&forge_dep, &aside), [aside.join("athena.jar")]);
        let neo_dep = l("Mod create requires athena 4.0 or above\nCurrently, athena is not installed");
        assert_eq!(missing_deps(&neo_dep, &aside), [aside.join("athena.jar")]);
        assert!(missing_deps(&l("Mod create requires athenaplus 1.0"), &aside).is_empty(), "whole ids only");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn without_mods_is_vanilla() {
        let d = std::env::temp_dir().join(format!("octo-nomods-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let g = d.join("inst/minecraft");
        std::fs::create_dir_all(g.join("saves/W")).unwrap();
        std::fs::create_dir_all(g.join("mods")).unwrap();
        level_dat(&g.join("saves/W/level.dat"), "1.20.1");
        std::fs::write(
            d.join("inst/mmc-pack.json"),
            r#"{"components":[{"uid":"net.minecraft","version":"1.20.1"},{"uid":"net.minecraftforge","version":"47.2.0"}]}"#,
        )
        .unwrap();
        jar(&g.join("mods/create.jar"), "META-INF/mods.toml", "");
        let out = d.join("server");
        std::fs::create_dir_all(&out).unwrap();
        assert_eq!(import(&g.join("saves/W"), &out, false, &Log::default()).unwrap(), (Flavor::Vanilla, "1.20.1".into(), None));
        assert!(out.join("world/level.dat").exists() && !out.join("mods").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Make a server from a real world and test-start it (on a free port). OCTO_PUT_BACK=1 first puts
    /// every left-out client-only mod back, to watch the test start sort them out:
    /// OCTO_DATA_DIR=<scratch> OCTO_WORLD=<world folder> cargo test boots_real_world -- --ignored --nocapture
    #[test]
    #[ignore]
    fn boots_real_world() {
        let (Some(_), Some(world)) = (std::env::var_os("OCTO_DATA_DIR"), std::env::var_os("OCTO_WORLD")) else {
            panic!("set OCTO_DATA_DIR (scratch) and OCTO_WORLD")
        };
        let log = Log::default();
        let dir = crate::server::servers_dir().join("world-e2e");
        let r = crate::server::create_with("world-e2e", 6144, &log, |dir, log| import(Path::new(&world), dir, true, log)).and_then(|_| {
            if std::env::var_os("OCTO_PUT_BACK").is_some() {
                for j in jars(&dir.join(CLIENT_ONLY)) {
                    std::fs::rename(&j, dir.join("mods").join(j.file_name().unwrap())).unwrap();
                }
            }
            test_start(&dir, &log)
        });
        log.lock().unwrap().iter().for_each(|l| println!("{l}"));
        r.unwrap();
        let props = std::fs::read_to_string(dir.join("server.properties")).unwrap();
        assert_eq!(crate::dashboard::props_get(&props, "server-port").as_deref(), Some("25565"), "port restored");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The client-only filter on a real mods folder: OCTO_MODS=<mods folder> cargo test filters_real_mods -- --ignored --nocapture
    #[test]
    #[ignore]
    fn filters_real_mods() {
        let mods = PathBuf::from(std::env::var_os("OCTO_MODS").expect("set OCTO_MODS"));
        let out = std::env::temp_dir().join(format!("octo-filter-{}", std::process::id()));
        let log = Log::default();
        let r = copy_mods(&mods, &out, &log);
        log.lock().unwrap().iter().for_each(|l| println!("{l}"));
        println!("Left out: {}", r.unwrap().join(", "));
        std::fs::remove_dir_all(&out).unwrap();
    }

    /// What this PC has: cargo test worlds_on_this_pc -- --ignored --nocapture
    #[test]
    #[ignore]
    fn worlds_on_this_pc() {
        for w in scan() {
            let g = game_dir(&w.dir).unwrap();
            println!("{} | {} | {:?} | level {:?}", w.name, w.place, from_instance(g), level_version(&w.dir.join("level.dat")));
        }
    }

    /// A Prism Fabric instance: world, a server mod, a client-only mod, configs.
    /// Talks to Modrinth (the client-only check), so it's ignored by default.
    #[test]
    #[ignore]
    fn e2e_world_with_mods() {
        let d = std::env::temp_dir().join(format!("octo-world-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let g = d.join("inst/minecraft");
        std::fs::create_dir_all(g.join("saves/My World")).unwrap();
        std::fs::create_dir_all(g.join("mods")).unwrap();
        std::fs::create_dir_all(g.join("config")).unwrap();
        std::fs::write(g.join("config/lithium.properties"), "x").unwrap();
        level_dat(&g.join("saves/My World/level.dat"), "1.21.1");
        std::fs::write(
            d.join("inst/mmc-pack.json"),
            r#"{"components":[{"uid":"net.minecraft","version":"1.21.1"},{"uid":"net.fabricmc.fabric-loader","version":"0.16.5"}]}"#,
        )
        .unwrap();
        jar(&g.join("mods/lithium.jar"), "fabric.mod.json", r#"{"id":"lithium"}"#);
        jar(&g.join("mods/minimap.jar"), "fabric.mod.json", r#"{"id":"minimap","environment":"client"}"#);
        let out = d.join("server");
        std::fs::create_dir_all(&out).unwrap();
        let got = import(&g.join("saves/My World"), &out, true, &Log::default()).unwrap();
        assert_eq!(got, (Flavor::Fabric, "1.21.1".into(), Some("0.16.5".into())));
        assert!(out.join("world/level.dat").exists());
        assert!(out.join("mods/lithium.jar").exists());
        assert!(!out.join("mods/minimap.jar").exists());
        assert!(out.join("config/lithium.properties").exists());
        std::fs::remove_dir_all(&d).unwrap();

        // the same instance exported as a client pack: refused unless cleaning was agreed to
        let pack = d.join("pack");
        std::fs::create_dir_all(pack.join(".minecraft/mods")).unwrap();
        std::fs::create_dir_all(pack.join(".minecraft/config")).unwrap();
        std::fs::write(pack.join("instance.cfg"), "").unwrap();
        std::fs::write(
            pack.join("mmc-pack.json"),
            r#"{"components":[{"uid":"net.minecraft","version":"1.21.1"},{"uid":"net.fabricmc.fabric-loader","version":"0.16.5"}]}"#,
        )
        .unwrap();
        std::fs::write(pack.join(".minecraft/config/lithium.properties"), "x").unwrap();
        jar(&pack.join(".minecraft/mods/lithium.jar"), "fabric.mod.json", r#"{"id":"lithium"}"#);
        jar(&pack.join(".minecraft/mods/minimap.jar"), "fabric.mod.json", r#"{"id":"minimap","environment":"client"}"#);
        let src = pack.to_string_lossy().into_owned();
        for (clean, out) in [(false, d.join("a")), (true, d.join("b"))] {
            std::fs::create_dir_all(&out).unwrap();
            let got = crate::packs::import(&src, &out, None, clean, &Log::default());
            if !clean {
                assert_eq!(got.unwrap_err(), crate::packs::CLIENT_PACK);
                continue;
            }
            assert_eq!(got.unwrap(), (Flavor::Fabric, "1.21.1".into(), Some("0.16.5".into())));
            assert!(out.join("mods/lithium.jar").exists());
            assert!(out.join(CLIENT_ONLY).join("minimap.jar").exists());
            assert!(out.join("config/lithium.properties").exists());
            assert!(!out.join("_client").exists() && !out.join("mmc-pack.json").exists());
        }
        std::fs::remove_dir_all(&d).unwrap();

        // real jars: Sodium is client-only on Modrinth, Lithium runs on servers
        let sha = |slug: &str| {
            let v = crate::get_json(&format!("https://api.modrinth.com/v2/project/{slug}/version")).unwrap();
            v[0]["files"][0]["hashes"]["sha1"].as_str().unwrap().to_string()
        };
        let (sodium, lithium) = (sha("sodium"), sha("lithium"));
        assert_eq!(crate::sources::client_only_hashes(&[sodium.clone(), lithium, "0".repeat(40)]).unwrap(), [sodium]);
    }
}
