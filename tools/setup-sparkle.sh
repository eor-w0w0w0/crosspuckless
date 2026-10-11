#!/usr/bin/env bash
set -euo pipefail

# Official release archive and its GitHub release-asset SHA-256 digest. Update
# both together when upgrading Sparkle; never fetch an unpinned "latest" build.
sparkle_version="2.10.0"
sparkle_sha256="c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c"
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
distribution_dir="${CROSSPUCK_SPARKLE_DIR:-$root_dir/build/sparkle-$sparkle_version}"
archive_path="${CROSSPUCK_SPARKLE_ARCHIVE:-$root_dir/build/Sparkle-$sparkle_version.tar.xz}"

usage() {
  cat <<'USAGE'
Usage: tools/setup-sparkle.sh [--verify-only]

Download and verify the pinned official Sparkle distribution, then print its
absolute directory. CROSSPUCK_SPARKLE_ARCHIVE can select an offline archive
(the same pinned checksum is required). CROSSPUCK_SPARKLE_DIR can select an
already extracted distribution; its version and complete helper layout are
validated instead of downloading. --verify-only never downloads or extracts.
USAGE
}

verify_only="0"
case "${1:-}" in
  "") ;;
  --verify-only) verify_only="1" ;;
  --help|-h) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac
if [[ $# -gt 1 ]]; then
  usage >&2
  exit 2
fi

verify_distribution() {
  local directory="$1"
  local framework="$directory/Sparkle.framework"
  local framework_version

  for executable in Sparkle Autoupdate Updater.app/Contents/MacOS/Updater \
    XPCServices/Installer.xpc/Contents/MacOS/Installer \
    XPCServices/Downloader.xpc/Contents/MacOS/Downloader; do
    if [[ ! -x "$framework/Versions/B/$executable" ]]; then
      echo "Incomplete Sparkle distribution: $framework/Versions/B/$executable" >&2
      return 1
    fi
  done
  for name in generate_appcast generate_keys sign_update; do
    if [[ ! -x "$directory/bin/$name" ]]; then
      echo "Missing Sparkle release tool: $directory/bin/$name" >&2
      return 1
    fi
  done
  if [[ ! -L "$framework/Versions/Current" || ! -L "$framework/Sparkle" \
    || ! -L "$framework/Resources" || ! -s "$directory/LICENSE" ]]; then
    echo "Sparkle symlinks or license are missing: $directory" >&2
    return 1
  fi
  framework_version="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' \
    "$framework/Resources/Info.plist")"
  if [[ "$framework_version" != "$sparkle_version" ]]; then
    echo "Expected Sparkle $sparkle_version, found $framework_version: $directory" >&2
    return 1
  fi
}

verify_archive() {
  local actual_sha256
  actual_sha256="$(shasum -a 256 "$archive_path" | awk '{print $1}')"
  if [[ "$actual_sha256" != "$sparkle_sha256" ]]; then
    echo "Sparkle archive checksum mismatch: $archive_path" >&2
    echo "Expected SHA256: $sparkle_sha256" >&2
    echo "Actual SHA256:   $actual_sha256" >&2
    return 1
  fi
}

if [[ -n "${CROSSPUCK_SPARKLE_DIR:-}" ]]; then
  verify_distribution "$distribution_dir"
elif [[ -d "$distribution_dir" && -f "$archive_path" ]]; then
  verify_archive
  verify_distribution "$distribution_dir"
elif [[ "$verify_only" == "1" ]]; then
  echo "Pinned Sparkle distribution is not installed. Run tools/setup-sparkle.sh first." >&2
  exit 1
else
  mkdir -p "$(dirname "$archive_path")" "$(dirname "$distribution_dir")"
  temporary_dir="$(mktemp -d "$(dirname "$distribution_dir")/.sparkle-download.XXXXXX")"
  trap 'rm -rf "$temporary_dir"' EXIT
  if [[ ! -f "$archive_path" ]]; then
    if [[ -n "${CROSSPUCK_SPARKLE_ARCHIVE:-}" ]]; then
      echo "Offline Sparkle archive does not exist: $archive_path" >&2
      exit 1
    fi
    echo "Downloading Sparkle $sparkle_version..." >&2
    curl --fail --location --silent --show-error \
      "https://github.com/sparkle-project/Sparkle/releases/download/$sparkle_version/Sparkle-$sparkle_version.tar.xz" \
      --output "$temporary_dir/archive.tar.xz"
    mv "$temporary_dir/archive.tar.xz" "$archive_path"
  fi
  verify_archive
  mkdir "$temporary_dir/distribution"
  tar -xJf "$archive_path" -C "$temporary_dir/distribution"
  verify_distribution "$temporary_dir/distribution"
  rm -rf "$distribution_dir"
  mv "$temporary_dir/distribution" "$distribution_dir"
fi

cd "$distribution_dir"
pwd
