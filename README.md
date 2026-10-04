# Octo Servers

A desktop app for hosting Minecraft servers on your own PC, written in Rust. It runs on Windows and Linux.

It can create Vanilla, Paper, Fabric, Forge and NeoForge servers. If the right Java version isn't installed, it installs it: through winget (Temurin JRE) on Windows, or pacman, apt or dnf on Linux.

You can also start from a world you played in singleplayer. Octo looks for worlds in the Minecraft launcher, CurseForge, Prism Launcher, MultiMC, PolyMC, Modrinth, ATLauncher, GDLauncher, the FTB app and Technic, or you can pick a folder or zip yourself. If the world comes from a modded instance, the server gets the same loader and version, the instance's mods and configs, and moves client-only mods (like Sodium, Iris or minimaps) to a mods-client-only folder. Then it starts the server once to check the mods load: a mod that crashes it with client-only code is moved aside too, and a left-out mod that another mod needs is put back.

You can also start from a modpack. Search Modrinth, Technic or ATLauncher or browse CurseForge from inside the app, paste a CurseForge, Modrinth, Technic or ATLauncher link, or pick a server pack you already downloaded (a zip or an unzipped folder). For CurseForge packs it uses the pack's server files when the author provides them. The loader and Minecraft version are worked out from the pack.

Each server has a dashboard, a console, a players tab (ops, whitelist, bans), a settings tab for server.properties, and a mods or plugins tab. The world is backed up before each start, and the last 5 automatic backups are kept. You can import an existing world from a folder or a zip, such as a singleplayer save, a downloaded map or one of the backups. The playit.gg tunnel lets friends join without port forwarding.

No API keys are needed. CurseForge projects are looked up through cfwidget, and files come from curseforge.com and forgecdn.

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

To build the Windows version from Linux, install `mingw-w64-gcc`, run `rustup target add x86_64-pc-windows-gnu`, then:

```sh
cargo build --release --target x86_64-pc-windows-gnu
```

The result is `target/x86_64-pc-windows-gnu/release/octo.exe`, a single file that runs on Windows 10 and 11.

Servers are stored in `%APPDATA%\octo-servers\servers` on Windows and `~/.local/share/octo-servers/servers` on Linux.

## Tests

```sh
cargo test
```

The tests marked `#[ignore]` download real files and start real servers. Run them with `cargo test -- --ignored --nocapture`.
