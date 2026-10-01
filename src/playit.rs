//! playit.gg tunnel: claim the agent through playit's API, then run their daemon (playitd).
use crate::{Log, cmd, data_dir, download, push, s, spawn_logged};
use std::hash::{BuildHasher, Hasher};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RELEASES: &str = "https://github.com/playit-cloud/playit-agent/releases/latest/download";
const API: &str = "https://api.playit.gg";

/// What the dashboard shows for the public address.
#[derive(Clone, Default, PartialEq, Debug)]
pub enum Tunnel {
    #[default]
    Off,
    /// not ready yet; the text says why
    Waiting(String),
    /// the address friends type into Minecraft
    Ready(String),
}

#[derive(Default)]
pub struct Playit {
    pub log: Log,
    pub tunnel: Arc<Mutex<Tunnel>>,
    child: Arc<Mutex<Option<Child>>>,
    cancel: Arc<AtomicBool>,
    starting: Arc<AtomicBool>,
}

impl Playit {
    pub fn running(&self) -> bool {
        if self.starting.load(Ordering::SeqCst) {
            return true;
        }
        let mut c = self.child.lock().unwrap();
        if c.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None))) {
            push(&self.log, "[octo] playit stopped");
            *c = None;
            let bad_secret = self.log.lock().unwrap().iter().rev().take(5).any(|l| l.contains("secret is no longer valid"));
            if bad_secret {
                let _ = std::fs::remove_file(data_dir().join("playit").join("secret.txt"));
                push(&self.log, "[octo] The playit.gg link was removed. Turn the tunnel on again to link it.");
            }
        }
        c.is_some()
    }

    pub fn start(&self) {
        let (log, child, cancel, starting, tunnel) =
            (self.log.clone(), self.child.clone(), self.cancel.clone(), self.starting.clone(), self.tunnel.clone());
        cancel.store(false, Ordering::SeqCst);
        starting.store(true, Ordering::SeqCst);
        *tunnel.lock().unwrap() = Tunnel::Waiting("Starting playit.gg...".into());
        std::thread::spawn(move || {
            match run(&log, &cancel) {
                Ok((c, secret)) if !cancel.load(Ordering::SeqCst) => {
                    *child.lock().unwrap() = Some(c);
                    starting.store(false, Ordering::SeqCst);
                    watch_tunnel(&secret, &tunnel, &cancel, &log);
                }
                Ok((mut c, _)) => {
                    let _ = c.kill();
                }
                Err(e) => {
                    push(&log, format!("ERROR: {e}"));
                    *tunnel.lock().unwrap() = Tunnel::Waiting(format!("playit.gg didn't start: {e}"));
                }
            }
            starting.store(false, Ordering::SeqCst);
        });
    }

    pub fn stop(&self) {
        *self.tunnel.lock().unwrap() = Tunnel::Off;
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn run(log: &Log, cancel: &AtomicBool) -> Result<(Child, String), String> {
    let dir = data_dir().join("playit");
    std::fs::create_dir_all(&dir).map_err(s)?;
    let (asset, exe) = if cfg!(windows) { ("playit-windows-x86_64-signed.exe", "playitd.exe") } else { ("playit-linux-amd64", "playitd") };
    let bin = dir.join(exe);
    if !bin.exists() {
        push(log, "Downloading the playit.gg agent...");
        download(&format!("{RELEASES}/{asset}"), &bin)?;
        #[cfg(unix)]
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(s)?;
    }
    let secret_file = dir.join("secret.txt");
    let secret = match std::fs::read_to_string(&secret_file) {
        Ok(k) if !k.trim().is_empty() => k.trim().to_string(),
        _ => {
            let k = claim(log, cancel)?;
            std::fs::write(&secret_file, &k).map_err(s)?;
            k
        }
    };
    push(log, "Starting the playit.gg tunnel.");
    let mut c = cmd(&bin);
    // Own IPC path: the default (/run/playit, or the system pipe) needs root / clashes with an installed playit.
    let sock = if cfg!(windows) { r"\\.\pipe\octo-playitd".into() } else { dir.join("playitd.sock") };
    let _ = std::fs::remove_file(&sock);
    c.arg("--secret").arg(&secret).arg("--socket-path").arg(sock).current_dir(&dir);
    Ok((spawn_logged(c, log)?, secret))
}

/// Keep `tunnel` up to date while the agent runs, creating the Minecraft tunnel the first time
/// so nobody has to set it up on playit.gg by hand.
fn watch_tunnel(secret: &str, tunnel: &Mutex<Tunnel>, cancel: &AtomicBool, log: &Log) {
    let mut tried_create = false;
    // kept so a failed create stays on screen instead of being replaced by "waiting"
    let mut create_error: Option<String> = None;
    while !cancel.load(Ordering::SeqCst) {
        let state = match api_auth("/v1/agents/rundata", serde_json::json!({}), secret) {
            Ok(r) => match tunnel_state(&r) {
                Some(t) => t,
                None if tried_create => {
                    Tunnel::Waiting(create_error.clone().unwrap_or("Waiting for playit.gg to set up the tunnel...".into()))
                }
                None => {
                    tried_create = true;
                    push(log, "Creating a Minecraft tunnel on playit.gg.");
                    match create_tunnel(secret, r["data"]["agent_id"].as_str().unwrap_or("")) {
                        Ok(()) => Tunnel::Waiting("Creating the tunnel...".into()),
                        Err(e) => {
                            push(log, format!("ERROR: {e}"));
                            create_error = Some(e.clone());
                            Tunnel::Waiting(e)
                        }
                    }
                }
            },
            Err(e) => Tunnel::Waiting(format!("Couldn't reach playit.gg: {e}")),
        };
        let ready = matches!(state, Tunnel::Ready(_));
        if !cancel.load(Ordering::SeqCst) {
            *tunnel.lock().unwrap() = state;
        }
        // poll fast until the address shows up, then just keep an eye on it
        for _ in 0..if ready { 60 } else { 6 } {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// The Minecraft tunnel's state from a /v1/agents/rundata response, None if there isn't one yet.
fn tunnel_state(r: &serde_json::Value) -> Option<Tunnel> {
    let is_mc = |t: &&serde_json::Value| t["tunnel_type"] == "minecraft-java";
    let d = &r["data"];
    if let Some(t) = d["tunnels"].as_array().into_iter().flatten().find(is_mc) {
        return Some(match t["disabled_reason"].as_str() {
            Some(why) => Tunnel::Waiting(format!("The tunnel is disabled on playit.gg ({why}).")),
            None => Tunnel::Ready(t["display_address"].as_str().unwrap_or("").to_string()),
        });
    }
    let pending = d["pending"].as_array().into_iter().flatten().find(is_mc)?;
    Some(Tunnel::Waiting(format!("playit.gg is setting up the tunnel: {}", pending["status_msg"].as_str().unwrap_or("pending"))))
}

/// Uses /tunnels/create: the live API rejects every body for /v1/tunnels/create
/// ("failed to parse body"), while this older endpoint accepts agent keys.
fn create_tunnel(secret: &str, agent_id: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "name": "Minecraft (Octo Servers)",
        "tunnel_type": "minecraft-java",
        "port_type": "tcp",
        "port_count": 1,
        "origin": {"type": "agent", "data": {"agent_id": agent_id, "local_ip": "127.0.0.1", "local_port": 25565}},
        "enabled": true,
        "alloc": null,
        "firewall_id": null,
        "proxy_protocol": null
    });
    let r = api_auth("/tunnels/create", body, secret)?;
    match r["status"].as_str() {
        Some("success") => Ok(()),
        _ => Err(match r["data"].as_str().or(r["data"]["message"].as_str()).unwrap_or("unknown error") {
            "RequiresVerifiedAccount" => {
                "playit.gg wants you to verify your account email before it creates tunnels. Verify it, then turn the tunnel off and on."
                    .into()
            }
            other => format!("playit.gg couldn't create the tunnel ({other}). You can add a Minecraft Java tunnel at playit.gg."),
        }),
    }
}

fn api_auth(path: &str, body: serde_json::Value, secret: &str) -> Result<serde_json::Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let mut r = agent.post(format!("{API}{path}")).header("Authorization", format!("Agent-Key {secret}")).send_json(body).map_err(s)?;
    r.body_mut().read_json().map_err(s)
}

fn api(path: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    agent.post(format!("{API}{path}")).send_json(body).map_err(s)?.body_mut().read_json().map_err(s)
}

/// Same flow as `playit-cli claim`: user approves in the browser, we get a secret key.
fn claim(log: &Log, cancel: &AtomicBool) -> Result<String, String> {
    let code = format!("{:010x}", std::collections::hash_map::RandomState::new().build_hasher().finish() & 0xff_ffff_ffff);
    push(log, "Open this link to connect Octo Servers to your playit.gg account:");
    push(log, format!("  https://playit.gg/claim/{code}"));
    let mut last = String::new();
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        let r = api(
            "/claim/setup",
            serde_json::json!({"code": code, "agent_type": "self-managed", "version": concat!("octo-servers ", env!("CARGO_PKG_VERSION"))}),
        )?;
        let state = r["data"].as_str().unwrap_or("").to_string();
        if state != last {
            push(log, format!("[playit] {state}"));
            last = state.clone();
        }
        match state.as_str() {
            "UserAccepted" => break,
            "UserRejected" => return Err("setup was rejected in the browser".into()),
            _ => std::thread::sleep(Duration::from_secs(2)),
        }
    }
    loop {
        let r = api("/claim/exchange", serde_json::json!({"code": code}))?;
        if let Some(k) = r["data"]["secret_key"].as_str() {
            push(log, "playit agent connected.");
            return Ok(k.to_string());
        }
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Creates the Minecraft tunnel on the linked account through Octo's own code, then waits for
    /// its address. Uses the real saved key; run on purpose only:
    /// cargo test playit_live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn playit_live() {
        let secret = std::fs::read_to_string(data_dir().join("playit/secret.txt")).unwrap().trim().to_string();
        let r = api_auth("/v1/agents/rundata", json!({}), &secret).unwrap();
        if tunnel_state(&r).is_none() {
            create_tunnel(&secret, r["data"]["agent_id"].as_str().unwrap()).unwrap();
        }
        for _ in 0..30 {
            let st = tunnel_state(&api_auth("/v1/agents/rundata", json!({}), &secret).unwrap());
            println!("{st:?}");
            if let Some(Tunnel::Ready(addr)) = st {
                assert!(!addr.is_empty());
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        panic!("no address after 60s");
    }

    #[test]
    fn reads_tunnel_address() {
        let ready = json!({"status": "success", "data": {"agent_id": "a", "pending": [], "tunnels": [
            {"tunnel_type": "https", "display_address": "web.example"},
            {"tunnel_type": "minecraft-java", "display_address": "fun-cat.gl.joinmc.link", "disabled_reason": null}
        ]}});
        assert_eq!(tunnel_state(&ready), Some(Tunnel::Ready("fun-cat.gl.joinmc.link".into())));
        let pending = json!({"data": {"tunnels": [], "pending": [{"tunnel_type": "minecraft-java", "status_msg": "allocating"}]}});
        assert!(matches!(tunnel_state(&pending), Some(Tunnel::Waiting(m)) if m.contains("allocating")));
        let none = json!({"data": {"tunnels": [], "pending": []}});
        assert_eq!(tunnel_state(&none), None);
    }
}
