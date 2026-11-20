#!/bin/sh
# Builds and installs Luch for the current user: binaries, luch-kms capability, user service,
# GNOME extension. SPDX-License-Identifier: GPL-3.0-or-later
set -eu
cd "$(dirname "$0")"

for crate in luchd luchctl luch-kms; do
    cargo install --locked --path "$crate"
done
BIN="${CARGO_HOME:-$HOME/.cargo}/bin"

echo "Granting CAP_SYS_ADMIN to $BIN/luch-kms (needed for the 'fast' KMS mode; sudo asks for a password)"
sudo setcap cap_sys_admin+ep "$BIN/luch-kms" || echo "setcap failed: 'fast' mode won't work, portal modes will"

install -Dm644 contrib/luchd.service "$HOME/.config/systemd/user/luchd.service"
sed -i "s|%h/.cargo/bin/luchd|$BIN/luchd|" "$HOME/.config/systemd/user/luchd.service"
systemctl --user daemon-reload
systemctl --user enable luchd.service
systemctl --user restart luchd.service

EXT="$HOME/.local/share/gnome-shell/extensions/luch@toxblh.github.io"
if command -v gnome-shell >/dev/null 2>&1; then
    rm -rf "$EXT"
    cp -r gnome-extension/luch@toxblh.github.io "$EXT"
    gnome-extensions enable luch@toxblh.github.io 2>/dev/null ||
        echo "Log out and back in, then: gnome-extensions enable luch@toxblh.github.io"
fi

echo "Done. Try: luchctl"
