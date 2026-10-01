#!/bin/sh
# Builds Octo Servers and adds it to your app menu (Linux).
set -e
cd "$(dirname "$0")"
cargo build --release
mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/scalable/apps
install -m755 target/release/octo ~/.local/bin/octo
install -m644 assets/octo.svg ~/.local/share/icons/hicolor/scalable/apps/octo-servers.svg
cat > ~/.local/share/applications/octo-servers.desktop <<DESKTOP
[Desktop Entry]
Type=Application
Name=Octo Servers
Comment=Host Minecraft servers
Exec=$HOME/.local/bin/octo
Icon=octo-servers
Categories=Game;
Terminal=false
DESKTOP
update-desktop-database ~/.local/share/applications 2>/dev/null || true
echo "Installed. Launch 'Octo Servers' from your app menu, or run: octo"
