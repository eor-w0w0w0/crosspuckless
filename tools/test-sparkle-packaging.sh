#!/usr/bin/env bash
set -euo pipefail

# Isolated packaging smoke tests: no cargo compilation, Keychain access,
# signing, application launches, Finder operations, or GitHub publication.
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sparkle_dir="$("$root_dir/tools/setup-sparkle.sh" --verify-only)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/crosspuck-packaging-test.XXXXXX")"
trap 'rm -rf "$fixture"' EXIT
mkdir -p "$fixture/tools" "$fixture/bin" "$fixture/crates/crosspuck-app/Resources"
cp "$root_dir/tools/build-app.sh" "$root_dir/tools/build-dmg.sh" "$root_dir/tools/setup-sparkle.sh" "$fixture/tools/"
cp "$root_dir/crates/crosspuck-app/Info.plist" "$fixture/crates/crosspuck-app/Info.plist"
cp "$root_dir/LICENSE" "$root_dir/THIRD-PARTY-NOTICES.md" "$fixture/"
cat > "$fixture/Cargo.toml" <<'TOML'
[workspace.package]
version = "0.4.0"
TOML
cat > "$fixture/bin/rustup" <<'SH'
#!/usr/bin/env sh
printf '%s\n' x86_64-pc-windows-gnu
SH
cat > "$fixture/bin/cargo" <<'SH'
#!/usr/bin/env sh
set -eu
test "$MACOSX_DEPLOYMENT_TARGET" = 12.0
printf '%s\n' "$*" >> "$CROSSPUCK_TEST_FIXTURE/cargo-calls"
mkdir -p "$CROSSPUCK_TEST_FIXTURE/target/x86_64-pc-windows-gnu/release"
printf 'fixture DLL' > "$CROSSPUCK_TEST_FIXTURE/target/x86_64-pc-windows-gnu/release/hid.dll"
mkdir -p "$CROSSPUCK_TEST_FIXTURE/target/debug" "$CROSSPUCK_TEST_FIXTURE/target/release"
printf 'fixture executable' > "$CROSSPUCK_TEST_FIXTURE/target/debug/CrossPuck"
printf 'fixture executable' > "$CROSSPUCK_TEST_FIXTURE/target/release/CrossPuck"
SH
chmod +x "$fixture/bin/"* "$fixture/tools/"*.sh
export PATH="$fixture/bin:$PATH"
export CROSSPUCK_TEST_FIXTURE="$fixture"
export CROSSPUCK_SPARKLE_DIR="$sparkle_dir"
unset CROSSPUCK_BUILD_VERSION CROSSPUCK_SPARKLE_FEED_URL
# Fixtures must never inherit a maintainer's real signing identities.
unset CROSSPUCK_APP_SIGN_IDENTITY CROSSPUCK_DMG_SIGN_IDENTITY

expect_failure() {
  local label="$1"
  shift
  if "$@" > "$fixture/failure.log" 2>&1; then
    echo "FAIL: $label unexpectedly succeeded" >&2
    exit 1
  fi
  echo "PASS: $label"
}

CROSSPUCK_SPARKLE_PUBLIC_KEY="" "$fixture/tools/build-app.sh" debug > "$fixture/debug.log" 2>&1
plist="$fixture/target/debug/CrossPuck.app/Contents/Info.plist"
if /usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$plist" >/dev/null 2>&1; then
  echo "FAIL: missing debug key was not removed" >&2
  exit 1
fi
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$plist")" = 0.4.0
test "$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "$plist")" = 12.0
grep -q 'in-app updating will be disabled' "$fixture/debug.log"
framework="$fixture/target/debug/CrossPuck.app/Contents/Frameworks/Sparkle.framework"
test -L "$framework/Sparkle"
test -L "$framework/Versions/Current"
test -x "$framework/Updater.app/Contents/MacOS/Updater"
test -x "$framework/XPCServices/Installer.xpc/Contents/MacOS/Installer"
test -x "$framework/XPCServices/Downloader.xpc/Contents/MacOS/Downloader"
cmp "$sparkle_dir/LICENSE" "$fixture/target/debug/CrossPuck.app/Contents/Resources/Sparkle-LICENSE.txt"
echo "PASS: debug bundle contains full framework, symlinks, license, and macOS 12 deployment target"

before_calls="$(wc -l < "$fixture/cargo-calls")"
expect_failure "release without signing public key" env CROSSPUCK_SPARKLE_PUBLIC_KEY="" "$fixture/tools/build-app.sh" release
expect_failure "malformed public key" env CROSSPUCK_SPARKLE_PUBLIC_KEY="invalid" "$fixture/tools/build-app.sh" release
expect_failure "prerelease build version without numeric override" env CROSSPUCK_BUILD_VERSION=0.4.1-beta.1 "$fixture/tools/build-app.sh" debug
expect_failure "insecure feed URL" env CROSSPUCK_SPARKLE_FEED_URL=http://example.com/appcast.xml "$fixture/tools/build-app.sh" debug
expect_failure "multiline build version" env CROSSPUCK_BUILD_VERSION=$'0.4.1\n2' "$fixture/tools/build-app.sh" debug
expect_failure "multiline feed URL" env CROSSPUCK_SPARKLE_FEED_URL=$'https://example.com/appcast.xml\nhttps://example.com/other' "$fixture/tools/build-app.sh" debug
test "$(wc -l < "$fixture/cargo-calls")" = "$before_calls"
echo "PASS: invalid release configuration fails before cargo compilation"

# A fixture-only key exercises base64 configuration; never ship this key.
fixture_public_key="AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
CROSSPUCK_SPARKLE_PUBLIC_KEY="$fixture_public_key" \
  CROSSPUCK_BUILD_VERSION=0.4.1 \
  CROSSPUCK_SPARKLE_FEED_URL=https://example.com/appcast.xml \
  "$fixture/tools/build-app.sh" release > "$fixture/release.log" 2>&1
plist="$fixture/target/release/CrossPuck.app/Contents/Info.plist"
test "$(/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$plist")" = "$fixture_public_key"
test "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$plist")" = 0.4.1
test "$(/usr/libexec/PlistBuddy -c 'Print :SUFeedURL' "$plist")" = https://example.com/appcast.xml
echo "PASS: release key, build number, and HTTPS feed overrides are embedded"

# Exercise the DMG script using stubbed native tools; no volumes are mounted.
cat > "$fixture/bin/codesign" <<'SH'
#!/usr/bin/env sh
printf '%s\n' "$*" >> "$CROSSPUCK_TEST_FIXTURE/codesign-calls"
SH
cat > "$fixture/bin/hdiutil" <<'SH'
#!/usr/bin/env sh
if [ "$1" = attach ]; then
  mkdir -p "$CROSSPUCK_TEST_FIXTURE/volume"
  printf '/dev/fixture\tApple_HFS\t%s/volume\n' "$CROSSPUCK_TEST_FIXTURE"
else
  for last_argument in "$@"; do :; done
  case "$1" in create|convert) touch "$last_argument" ;; esac
fi
SH
cat > "$fixture/bin/osascript" <<'SH'
#!/usr/bin/env sh
cat > /dev/null
SH
cat > "$fixture/bin/swift" <<'SH'
#!/usr/bin/env sh
touch "$2"
SH
chmod +x "$fixture/bin/"*
"$fixture/tools/build-dmg.sh" --app "$fixture/target/release/CrossPuck.app" \
  --app-sign-identity fixture-identity --output "$fixture/test.dmg" \
  > "$fixture/dmg.log" 2>&1
python3 - "$fixture/codesign-calls" <<'PY'
import sys
from pathlib import Path
commands = Path(sys.argv[1]).read_text().splitlines()
signed = [line for line in commands if '--sign fixture-identity' in line]
expected = ['Installer.xpc', 'Downloader.xpc', '/Autoupdate', '/Updater.app',
            '/Sparkle.framework', '/CrossPuck.app']
assert len(signed) == len(expected), signed
for line, suffix in zip(signed, expected):
    assert line.endswith(suffix), line
assert '--preserve-metadata=entitlements' in signed[1]
assert all('--deep' not in line for line in signed)
assert any('--verify --deep --strict' in line for line in commands)
PY
echo "PASS: DMG signs nested helpers inside-out, preserving Downloader entitlements"
/usr/libexec/PlistBuddy -c 'Delete :SUPublicEDKey' "$plist"
expect_failure "prebuilt release DMG without public key" \
  "$fixture/tools/build-dmg.sh" --app "$fixture/target/release/CrossPuck.app" \
  --output "$fixture/invalid.dmg"

expect_failure "incomplete framework override" env CROSSPUCK_SPARKLE_DIR="$fixture" "$fixture/tools/setup-sparkle.sh" --verify-only
printf 'bad archive' > "$fixture/bad.tar.xz"
expect_failure "tampered offline archive" env -u CROSSPUCK_SPARKLE_DIR \
  CROSSPUCK_SPARKLE_ARCHIVE="$fixture/bad.tar.xz" "$fixture/tools/setup-sparkle.sh"
echo "Sparkle packaging smoke checks passed."
