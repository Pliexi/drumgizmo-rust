#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# cargo test --workspace with a summary; exit non-zero on any failure.

set -uo pipefail
cd "$(dirname "$0")/.."

echo "== cargo test --workspace =="
cargo test --workspace --all-features
status=$?

echo
if [ $status -eq 0 ]; then
    echo "all workspace tests passed"
else
    echo "TEST FAILURES above (exit $status)" >&2
fi
exit $status
