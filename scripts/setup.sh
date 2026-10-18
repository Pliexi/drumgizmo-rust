#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# set -euxo pipefail
# Development environment check + dependency fetch for drumgizmo-rust.

cd "$(dirname "$0")/.."

echo "== toolchain =="
if ! command -v cargo >/dev/null; then
    echo "cargo not found: install rustup (https://rustup.rs) and try again" >&2
    exit 1
fi
rustc --version
cargo --version

if ! cargo fmt --version >/dev/null 2>&1; then
    echo "rustfmt missing: run 'rustup component add rustfmt'" >&2
    exit 1
fi
if ! cargo clippy --version >/dev/null 2>&1; then
    echo "clippy missing: run 'rustup component add clippy'" >&2
    exit 1
fi

echo "== fetching dependencies =="
cargo fetch

echo "== bundle skeleton check =="
test -f crates/lv2_plugin/drumgizmo-rust.lv2/manifest.ttl \
    || { echo "LV2 bundle manifest missing" >&2; exit 1; }
test -f crates/vst3_plugin/drumgizmo-rust.vst3/Contents/moduleinfo.json \
    || { echo "VST3 bundle metadata missing" >&2; exit 1; }

echo
echo "setup complete: run scripts/build.sh next"
