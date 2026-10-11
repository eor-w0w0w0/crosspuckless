#!/usr/bin/env sh
set -eu

profile="${1:-debug}"
root_dir="$(cd "$(dirname "$0")/.." && pwd)"
driver_target="x86_64-pc-windows-gnu"
driver_profile="release"
app_features="${CROSSPUCK_APP_FEATURES:-}"
# Sparkle 2.10 and the app must agree on the supported operating system.
export MACOSX_DEPLOYMENT_TARGET=12.0

ensure_driver_target_installed() {
  if ! command -v rustup >/dev/null 2>&1; then
    cat >&2 <<EOF
rustup is required to verify the Windows target before building the guest driver.

Install rustup first, then add the driver target:
  rustup target add $driver_target
EOF
    exit 1
  fi

  if ! rustup target list --installed | grep -Fxq "$driver_target"; then
    cat >&2 <<EOF
Required Rust target is not installed:
  $driver_target

Install it first:
  rustup target add $driver_target
EOF
    exit 1
  fi
}

workspace_version() {
  awk '
    /^\[workspace.package\]/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && /^[[:space:]]*version[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*"/, "", value)
      sub(/".*$/, "", value)
      print value
      exit
    }
  ' "$root_dir/Cargo.toml"
}

set_bundle_version() {
  plist="$1"
  version="$2"

  if [ ! -x /usr/libexec/PlistBuddy ]; then
    echo "PlistBuddy is required to update the app bundle version." >&2
    exit 1
  fi

  /usr/libexec/PlistBuddy \
    -c "Set :CFBundleShortVersionString $version" \
    "$plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build_version" "$plist"
}

set_plist_string() {
  plist="$1"
  key="$2"
  value="$3"
  /usr/libexec/PlistBuddy -c "Delete :$key" "$plist" >/dev/null 2>&1 || true
  /usr/libexec/PlistBuddy -c "Add :$key string $value" "$plist"
}

valid_public_key() {
  # Ed25519 public keys are 32 bytes, in canonical base64 (44 characters).
  printf '%s' "$1" | grep -Eq '^[A-Za-z0-9+/]{43}=$' || return 1
  decoded_size="$(printf '%s' "$1" | base64 -D 2>/dev/null | wc -c | tr -d '[:space:]')"
  [ "$decoded_size" = "32" ] || return 1
  canonical_key="$(printf '%s' "$1" | base64 -D 2>/dev/null | base64 | tr -d '\n')"
  [ "$canonical_key" = "$1" ]
}

sha256_file() {
  shasum -a 256 "$1" | awk '{print $1}'
}

file_size() {
  wc -c < "$1" | tr -d '[:space:]'
}

case "$profile" in
  debug)
    app_cargo_args=""
    ;;
  release)
    app_cargo_args="--release"
    ;;
  *)
    echo "usage: $0 [debug|release]" >&2
    exit 2
    ;;
esac

app_version="$(workspace_version)"
if [ -z "$app_version" ]; then
  echo "Could not read workspace package version from Cargo.toml" >&2
  exit 1
fi

build_version="${CROSSPUCK_BUILD_VERSION:-$app_version}"
numeric_build_version="$(printf '%s' "$build_version" | tr -cd '0-9.')"
if [ "$numeric_build_version" != "$build_version" ] || \
  ! printf '%s' "$build_version" | grep -Eq '^(0|[1-9][0-9]*)(\.(0|[1-9][0-9]*)){0,2}$'; then
  echo "Invalid CFBundleVersion: $build_version" >&2
  echo "Use 1-3 numeric components; prereleases require CROSSPUCK_BUILD_VERSION." >&2
  exit 1
fi

source_plist="$root_dir/crates/crosspuck-app/Info.plist"
if [ "${CROSSPUCK_SPARKLE_PUBLIC_KEY+x}" = "x" ]; then
  sparkle_public_key="$CROSSPUCK_SPARKLE_PUBLIC_KEY"
else
  sparkle_public_key="$(/usr/libexec/PlistBuddy -c 'Print :SUPublicEDKey' "$source_plist" 2>/dev/null || true)"
fi
if ! valid_public_key "$sparkle_public_key"; then
  if [ "$profile" = "release" ]; then
    echo "A valid Sparkle Ed25519 public key is required for release builds." >&2
    echo "Set SUPublicEDKey in Info.plist or CROSSPUCK_SPARKLE_PUBLIC_KEY (32-byte base64 key)." >&2
    exit 1
  fi
  echo "Warning: no valid Sparkle public key; in-app updating will be disabled in this debug bundle." >&2
  sparkle_public_key=""
fi
sparkle_feed_url="${CROSSPUCK_SPARKLE_FEED_URL:-$(/usr/libexec/PlistBuddy -c 'Print :SUFeedURL' "$source_plist" 2>/dev/null || true)}"
if [ -z "$sparkle_feed_url" ]; then
  sparkle_feed_url="https://github.com/scryner/crosspuck/releases/latest/download/appcast.xml"
fi
single_line_feed="$(printf '%s' "$sparkle_feed_url" | tr -d '\r\n')"
if [ "$single_line_feed" != "$sparkle_feed_url" ] || \
  ! printf '%s' "$sparkle_feed_url" | grep -Eq '^https://[^[:space:]"<>]+$'; then
  echo "Sparkle feed URL must use HTTPS without whitespace: $sparkle_feed_url" >&2
  exit 1
fi
sparkle_dir="$("$root_dir/tools/setup-sparkle.sh")"

ensure_driver_target_installed

cargo build \
  --manifest-path "$root_dir/Cargo.toml" \
  -p crosspuck-driver \
  --release \
  --target "$driver_target"

if [ -n "$app_features" ]; then
  # shellcheck disable=SC2086
  cargo build --manifest-path "$root_dir/Cargo.toml" -p crosspuck-app $app_cargo_args --features "$app_features"
else
  # shellcheck disable=SC2086
  cargo build --manifest-path "$root_dir/Cargo.toml" -p crosspuck-app $app_cargo_args
fi

driver_dll="$root_dir/target/$driver_target/$driver_profile/hid.dll"
if [ ! -f "$driver_dll" ]; then
  echo "driver DLL not found after build: $driver_dll" >&2
  exit 1
fi

app_dir="$root_dir/target/$profile/CrossPuck.app"
contents_dir="$app_dir/Contents"
macos_dir="$contents_dir/MacOS"
resources_dir="$contents_dir/Resources"
frameworks_dir="$contents_dir/Frameworks"
guest_driver_dir="$resources_dir/GuestDriver"

rm -rf "$app_dir"
mkdir -p "$macos_dir" "$resources_dir" "$guest_driver_dir" "$frameworks_dir"
cp "$root_dir/target/$profile/CrossPuck" "$macos_dir/CrossPuck"
cp "$root_dir/crates/crosspuck-app/Info.plist" "$contents_dir/Info.plist"
set_bundle_version "$contents_dir/Info.plist" "$app_version"
set_plist_string "$contents_dir/Info.plist" LSMinimumSystemVersion 12.0
set_plist_string "$contents_dir/Info.plist" SUFeedURL "$sparkle_feed_url"
if [ -n "$sparkle_public_key" ]; then
  set_plist_string "$contents_dir/Info.plist" SUPublicEDKey "$sparkle_public_key"
else
  /usr/libexec/PlistBuddy -c 'Delete :SUPublicEDKey' "$contents_dir/Info.plist" >/dev/null 2>&1 || true
fi
# Preserve the versioned framework's symlinks and its executable helpers.
ditto "$sparkle_dir/Sparkle.framework" "$frameworks_dir/Sparkle.framework"
cp -R "$root_dir/crates/crosspuck-app/Resources/." "$resources_dir/"
cp "$root_dir/LICENSE" "$resources_dir/LICENSE"
cp "$root_dir/THIRD-PARTY-NOTICES.md" "$resources_dir/THIRD-PARTY-NOTICES.md"
cp "$sparkle_dir/LICENSE" "$resources_dir/Sparkle-LICENSE.txt"
cp "$driver_dll" "$guest_driver_dir/hid.dll"

driver_sha256="$(sha256_file "$guest_driver_dir/hid.dll")"
driver_size="$(file_size "$guest_driver_dir/hid.dll")"
cat > "$guest_driver_dir/manifest.json" <<EOF
{
  "name": "crosspuck-driver",
  "dll_name": "hid.dll",
  "target": "$driver_target",
  "profile": "$driver_profile",
  "sha256": "$driver_sha256",
  "size": $driver_size
}
EOF

echo "$app_dir"
