#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Launch the standalone app. Extra arguments go straight to the binary
# (see --help for the CLI contract). Use --debug to run the dev build.

set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${DG_PROFILE:-release}"
BIN="target/${PROFILE}/drumgizmo-rust"
if ! command -v "$BIN" >/dev/null 2>&1; then
    echo "$BIN not built yet; building..." >&2
    scripts/build.sh "$1" >/dev/null
fi
exec "$BIN" "${@:2}"
