#!/usr/bin/env bash
set -euo pipefail

# Exercise the real Rust/AppKit/Sparkle menu without launching the host service,
# using production preferences, downloading updates, or replacing an app.
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sparkle_dir="$("$root_dir/tools/setup-sparkle.sh")"
cargo build --manifest-path "$root_dir/Cargo.toml" -p crosspuck-app --example sparkle_smoke
fixture_dir="$(mktemp -d "${TMPDIR:-/tmp}/crosspuck-sparkle-menu.XXXXXX")"
fixture_app="$fixture_dir/SparkleSmoke.app"
fixture_exe="$fixture_app/Contents/MacOS/SparkleSmoke"
cleanup() {
  if [[ -x "$fixture_exe" ]]; then
    "$fixture_exe" cleanup >/dev/null 2>&1 || true
  fi
  rm -rf "$fixture_dir"
}
trap cleanup EXIT
mkdir -p "$fixture_app/Contents/MacOS" "$fixture_app/Contents/Frameworks"
cp "$root_dir/target/debug/examples/sparkle_smoke" "$fixture_exe"
python3 - "$root_dir/crates/crosspuck-app/Info.plist" "$fixture_app/Contents/Info.plist" <<'PY'
import plistlib
import sys
import uuid

with open(sys.argv[1], "rb") as source:
    plist = plistlib.load(source)
identifier = "com.github.scryner.crosspuck.sparkle-smoke." + uuid.uuid4().hex
plist.update(CFBundleIdentifier=identifier, CFBundleExecutable="SparkleSmoke",
             CFBundleVersion="0.4.0")
with open(sys.argv[2], "wb") as output:
    plistlib.dump(plist, output)
PY

"$fixture_exe" unavailable
ditto "$sparkle_dir/Sparkle.framework" "$fixture_app/Contents/Frameworks/Sparkle.framework"
"$fixture_exe" persist
"$fixture_exe" persisted
"$fixture_exe"
echo "Real Sparkle/Rust menu lifecycle and cross-launch preference checks passed."
