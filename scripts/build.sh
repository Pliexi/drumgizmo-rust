#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# cargo build --release + plugin bundle staging (LV2/VST3 shared objects
# are copied next to their TTL/metadata descriptions).

set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-release}"
case "$MODE" in
    --debug|debug) CARGO_TARGET_DIR_FLAG=; PROFILE_DIR="debug" ;;
    *) PROFILE_DIR="release" ;;
esac

echo "== cargo build --${PROFILE_DIR} =="
if [ "$PROFILE_DIR" = release ]; then
    cargo build --release
else
    cargo build
fi

STAGE_DIR="target/staged"
mkdir -p "$STAGE_DIR"

echo "== staging LV2 bundle =="
LV2_BUNDLE_DIR="crates/lv2_plugin/drumgizmo-rust.lv2"
LV2_LIB="target/${PROFILE_DIR}/libdg_lv2_plugin.so"
test -f "$LV2_LIB" || { echo "missing $LV2_LIB" >&2; exit 1; }
cp "$LV2_LIB" "$LV2_BUNDLE_DIR/"
STAGE_DIR="$STAGE_DIR" cp -r "$LV2_BUNDLE_DIR" "$STAGE_DIR/"

echo "== staging VST3 bundle =="
VST3_BUNDLE_DIR="crates/vst3_plugin/drumgizmo-rust.vst3"
VST3_LIB="target/${PROFILE_DIR}/libdg_vst3_plugin.so"
test -f "$VST3_LIB" || { echo "missing $VST3_LIB" >&2; exit 1; }
ARCH_DIR="$(case "$(uname -m)" in x86_64) echo x86_64-linux;; aarch64) echo aarch64-linux;; *) uname -m-linux;; esac)"
mkdir -p "$VST3_BUNDLE_DIR/Contents/$ARCH_DIR"
cp "$VST3_LIB" "$VST3_BUNDLE_DIR/Contents/$ARCH_DIR/"
cp -r "$VST3_BUNDLE_DIR" "$STAGE_DIR/"

echo "== CLAP =="
CLAP_LIB="target/${PROFILE_DIR}/libdg_clap_plugin.so"
test -f "$CLAP_LIB" && cp "$CLAP_LIB" "$STAGE_DIR/"

echo
echo "artifacts:"
ls -1 "$STAGE_DIR"
echo
echo "install: scripts/install_local.sh"
