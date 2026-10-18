#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# cargo build --release + plugin bundle staging (LV2/VST3 shared objects
# are staged next to their TTL/metadata descriptions under target/staged),
# plus the standalone binary and the CLAP binary.
#
# Staging never writes into the in-tree bundle directories; the compiled
# bundles appear only in target/staged. The VST3 .so is named after the
# bundle stem ("drumgizmo-rust.so"), which is what hosts require on Linux.

set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-release}"
case "$MODE" in
    --debug|debug) PROFILE_DIR="debug" ;;
    *) PROFILE_DIR="release" ;;
esac

echo "== cargo build --${PROFILE_DIR} =="
if [ "$PROFILE_DIR" = release ]; then
    cargo build --release
else
    cargo build
fi

STAGE_DIR="target/staged"
rm -rf "$STAGE_DIR"
mkdir -p "$STAGE_DIR"

echo "== staging LV2 bundle =="
LV2_LIB="target/${PROFILE_DIR}/libdg_lv2_plugin.so"
test -f "$LV2_LIB" || { echo "missing $LV2_LIB" >&2; exit 1; }
LV2_STAGE="$STAGE_DIR/drumgizmo-rust.lv2"
mkdir -p "$LV2_STAGE"
cp crates/lv2_plugin/drumgizmo-rust.lv2/manifest.ttl "$LV2_STAGE/"
cp crates/lv2_plugin/drumgizmo-rust.lv2/drumgizmo-rust.ttl "$LV2_STAGE/"
cp "$LV2_LIB" "$LV2_STAGE/libdg_lv2_plugin.so"

echo "== staging VST3 bundle =="
VST3_LIB="target/${PROFILE_DIR}/libdg_vst3_plugin.so"
test -f "$VST3_LIB" || { echo "missing $VST3_LIB" >&2; exit 1; }
ARCH_DIR="$(case "$(uname -m)" in x86_64) echo x86_64-linux;; aarch64) echo aarch64-linux;; *) echo "$(uname -m)-linux";; esac)"
VST3_STAGE="$STAGE_DIR/drumgizmo-rust.vst3"
mkdir -p "$VST3_STAGE/Contents/$ARCH_DIR"
cp "$VST3_LIB" "$VST3_STAGE/Contents/$ARCH_DIR/drumgizmo-rust.so"
cp crates/vst3_plugin/drumgizmo-rust.vst3/Contents/moduleinfo.json \
   "$VST3_STAGE/Contents/moduleinfo.json"

echo "== CLAP =="
CLAP_LIB="target/${PROFILE_DIR}/libdg_clap_plugin.so"
test -f "$CLAP_LIB" && cp "$CLAP_LIB" "$STAGE_DIR/libdg_clap_plugin.so"

echo
echo "artifacts:"
ls -1 "$STAGE_DIR"
echo
echo "install: scripts/install_local.sh"
