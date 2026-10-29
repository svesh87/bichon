#!/bin/sh
# Checks of the fork's own code (crates/server/src/mcp and its wiring):
# rustfmt, clippy without warnings, the MCP tests, line coverage >= MIN_COVERAGE.
# Upstream code is not judged here: it has warnings of its own.
#
# Needs: rustfmt, clippy, llvm-tools-preview, cargo-llvm-cov, python3; and web/dist
# (or pnpm) for the server's build script.
set -eu

MIN_COVERAGE=${MIN_COVERAGE:-80}
OURS='crates/server/src/mcp/'
WIRING='crates/server/src/rest/mod.rs|crates/server/src/lib.rs|crates/core/src/users/permissions.rs'

echo "== rustfmt"
rustfmt --edition 2021 --check crates/server/src/mcp/*.rs 2>/dev/null

echo "== clippy"
log=$(mktemp)
cargo clippy --locked -p bichon-server --all-targets --message-format=short >"$log" 2>&1 || {
    cat "$log"
    exit 1
}
if grep -E "^($OURS|$WIRING)" "$log"; then
    echo "clippy: warnings in the fork's code"
    exit 1
fi

echo "== tests + coverage"
cargo llvm-cov --locked -p bichon-server --lib --json --summary-only --output-path cov.json -- mcp::
python3 -I - "$MIN_COVERAGE" <<'EOF'
import json, sys
minimum = float(sys.argv[1])
files = [f for f in json.load(open("cov.json"))["data"][0]["files"]
         if "/crates/server/src/mcp/" in f["filename"] and not f["filename"].endswith("/tests.rs")]
covered = sum(f["summary"]["lines"]["covered"] for f in files)
count = sum(f["summary"]["lines"]["count"] for f in files)
for f in files:
    lines = f["summary"]["lines"]
    print(f"  {f['filename'].split('/src/', 1)[-1]}: {lines['percent']:.1f}% of {lines['count']} lines")
percent = 100.0 * covered / count
print(f"MCP line coverage: {percent:.1f}% (minimum {minimum:.0f}%)")
sys.exit(0 if percent >= minimum else 1)
EOF
