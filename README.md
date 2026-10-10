# Octo Servers

A desktop app for hosting Minecraft servers on your own PC, written in Rust. It runs on Windows and Linux.

It can create Vanilla, Paper, Fabric, Forge and NeoForge servers. If the right Java version isn't installed, it installs it: through winget (Temurin JRE) on Windows, or pacman, apt or dnf on Linux.

You can also start from a world you played in singleplayer. Octo looks for worlds in the Minecraft launcher, CurseForge, Prism Launcher, MultiMC, PolyMC, Modrinth, ATLauncher, GDLauncher, the FTB app and Technic, or you can pick a folder or zip yourself. If the world comes from a modded instance, the server gets the same loader and version, the instance's mods and configs, and moves client-only mods (like Sodium, Iris or minimaps) to a mods-client-only folder. Then it starts the server once to check the mods load: a mod that crashes it with client-only code is moved aside too, and a left-out mod that another mod needs is put back.

You can also start from a modpack. Search Modrinth, Technic or ATLauncher or browse CurseForge from inside the app, paste a CurseForge, Modrinth, Technic or ATLauncher link, or pick a server pack you already downloaded (a zip or an unzipped folder). Packs found through search only install as servers: a CurseForge pack needs server files from its author, and one without them is refused. The loader and Minecraft version are worked out from the pack. If you import the client version of a pack yourself (a CurseForge pack without server files, or a Prism, MultiMC or ATLauncher export), Octo asks whether to clean and import it. If you say yes, it gets the same treatment as a modded world: the instance's loader, mods and configs, with client-only mods moved to mods-client-only.

Each server has a dashboard, a console, a players tab (ops, whitelist, bans), a settings tab for server.properties, and a mods or plugins tab. The world is backed up before each start, and the last 5 automatic backups are kept. You can import an existing world from a folder or a zip, such as a singleplayer save, a downloaded map or one of the backups. The playit.gg tunnel lets friends join without port forwarding.

Right-click a server in the list to delete, export (zip the server folder, minus its backups) or rename it. The settings tab has a Delete world button, which backs the world up first; the server makes a new one on its next start.

No API keys are needed. CurseForge projects are looked up through cfwidget. If it cannot resolve a link, Octo loads that project's page in its hidden browser to read the project ID. Files come from curseforge.com and forgecdn. Browser verification can still block a lookup; Octo reports the failure so you can retry later.

## Install on Linux

```sh
./install.sh
```

This builds the app, copies it to `~/.local/bin/octo` and adds Octo Servers to your app menu.

The New server dialog has separate Custom, Import, World, Modrinth and CurseForge options. Choose a provider, search, and select a modpack from Octo's result list. The Mods tab also lets you choose Modrinth or CurseForge. CurseForge searches run in a hidden native browser using normal JavaScript and a persistent cookie profile; the website isn't displayed. If browser verification blocks a search, Octo reports the failure and you can paste a project link instead.

## Build

The browser uses WebKitGTK on Linux and WebView2 on Windows. Linux needs GTK 3 and WebKitGTK 4.1 development packages (`libwebkit2gtk-4.1-dev` on Debian/Ubuntu, `webkit2gtk-4.1` on Arch). Octo uses X11, including XWayland on Wayland desktops. Windows needs the Microsoft Edge WebView2 Runtime.

```sh
cargo build --release
```

The result is `target/release/octo`.

To build the Windows version from Linux, install cargo-xwin, clang and llvm, run `rustup target add x86_64-pc-windows-msvc`, then:

```sh
cargo xwin build --release --target x86_64-pc-windows-msvc
```

The result is `target/x86_64-pc-windows-msvc/release/octo.exe`, a single file that runs on Windows 10 and 11 with no DLLs next to it. Don't release the `x86_64-pc-windows-gnu` build: it needs WebView2Loader.dll beside it.

## Where Octo keeps things

Everything Octo saves is in `%APPDATA%\octo-servers` on Windows and `~/.local/share/octo-servers` on Linux:

- `servers`: one folder per server, with its world, mods, backups and `octo.json`
- `playit`: the playit.gg agent and its secret key
- `browser-profile` and `browser-downloads`: the hidden CurseForge browser
- `settings.json`: app settings
- `crash.txt`: details of the last crash. Release builds on Windows have no console, so if Octo crashes it saves what happened here and shows where the file is.

To remove Octo completely, delete that folder. On Linux, also delete `~/.local/bin/octo`, `~/.local/share/applications/octo-servers.desktop` and `~/.local/share/icons/hicolor/scalable/apps/octo-servers.svg`. Deleting the playit folder doesn't remove the agent or tunnel from your playit.gg account; remove them on the website.

## Tests

```sh
cargo test
```

The tests marked `#[ignore]` download real files and start real servers. Run them with `cargo test -- --ignored --nocapture`.
