#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Install the compiled bundles and binary into user-local plugin paths
# ($HOME/.lv2, $HOME/.vst3, $HOME/.clap, $HOME/.local/bin). Linux only.

set -euo pipefail
cd "$(dirname "$0")/.."

echo "== fresh staging =="
scripts/build.sh >/dev/null
STAGE_DIR="target/staged"

install_dir() {
    local from="$1" base="$2" dest_name
    dest_name="${3:-$(basename "$2")}"
    local dest="$base/$dest_name"
    mkdir -p "$(dirname "$dest")"
    rm -rf "$dest"
    cp -r "$from" "$dest"
    echo "installed: $dest"
}

install_dir "$STAGE_DIR/drumgizmo-rust.lv2" "$HOME/.lv2"
install_dir "$STAGE_DIR/drumgizmo-rust.vst3" "$HOME/.vst3"
install_dir "$STAGE_DIR/libdg_clap_plugin.so" "$HOME/.clap" "drumgizmo-rust.clap"

BIN="target/release/drumgizmo-rust"
if command -v "$BIN" >/dev/null 2>&1; then
    mkdir -p "$HOME/.local/bin"
    cp "$BIN" "$HOME/.local/bin/"
    echo "installed: $HOME/.local/bin/drumgizmo-rust"
fi

cat <<'NOTE'

Installed. Your DAW should rescan its plugin list to pick them up.
LV2 hosts additionally need the bundle TTLs present (kept inside the bundle).

NOTE
