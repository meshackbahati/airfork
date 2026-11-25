#!/usr/bin/env bash
# install airfork locally. experimental, so this is intentionally dumb.
set -euo pipefail

cd "$(dirname "$0")"
cargo build --release

PREFIX="${PREFIX:-/usr/local}"

sudo install -Dm755 "target/release/airfork" "$PREFIX/bin/airfork"
install -Dm755 "target/release/airfork-gui" "$HOME/.local/bin/airfork-gui"

mkdir -p "$HOME/.local/share/applications"
cat > "$HOME/.local/share/applications/airfork.desktop" <<EOF
[Desktop Entry]
Name=airfork
Comment=experimental wifi share
Exec=$HOME/.local/bin/airfork-gui
Terminal=false
Type=Application
Categories=Network;
EOF

# pkexec uses this to ask once per session instead of every click
sudo tee /usr/share/polkit-1/actions/dev.airfork.policy >/dev/null <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN" "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <action id="dev.airfork.start">
    <description>start or stop the airfork hotspot</description>
    <message>airfork needs root to create a virtual access point</message>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_admin_keep</allow_active>
    </defaults>
    <annotate key="org.freedesktop.policykit.exec.path">$PREFIX/bin/airfork</annotate>
  </action>
</policyconfig>
EOF

sudo install -Dm755 scripts/99-airfork-wifi /etc/NetworkManager/dispatcher.d/99-airfork-wifi
/usr/bin/wpa_cli -i wlan0 set disable_btm 1 >/dev/null 2>&1 || true

echo "installed: $PREFIX/bin/airfork and ~/.local/bin/airfork-gui"
