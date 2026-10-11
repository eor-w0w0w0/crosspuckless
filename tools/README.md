# CrossPuck Tools

This directory contains install, smoke-test, logging, and reference-probe tools
used while developing and validating CrossPuck.

Shell entrypoints live directly under `tools/`. Deprecated CrossOver wrapper
scripts have been removed; use the current `tools/*.sh` commands below.

## CrossOver Install

The CrossPuck menu bar app is the preferred installer. It installs, updates,
and uninstalls the driver; install, update, and repair also import the generated
Wine loader override registry file with CrossOver `regedit`. Uninstall removes
only the app-local `hid.dll` and intentionally leaves Wine registry settings
unchanged. These shell commands are for manual development and smoke-test
workflows.

Copy the production guest driver next to `Steam.exe`:

```sh
tools/install-driver.sh --bottle Steam
```

To also import Wine's `hid=native,builtin` loader override:

```sh
tools/install-driver.sh --bottle Steam --override-dll
```

Useful options:

```sh
tools/install-driver.sh \
  --bottle Steam \
  --driver target/x86_64-pc-windows-gnu/release/hid.dll \
  --no-build
```

The script copies `hid.dll`, backs up any existing app-local `hid.dll`, and
initializes `crosspuck-driver.log` next to Steam. By default it does not call
CrossOver `regedit`. With `--override-dll`, it writes
`crosspuck-wine-override.reg` into the bottle and imports it with the matching
CrossOver app, using CrossOver Preview first for preview-marked bottles and
CrossOver first for regular bottles. It does not write guest runtime
`CROSSPUCK_*` registry or environment settings. Guest runtime settings remain
host-owned unless the macOS host app sends overrides over the bridge connection.

Do not copy this DLL into `drive_c/windows/system32`. The driver is intended to
be app-local and forwards non-virtual HID calls to the real system HID DLL.

## DMG Packaging

Build a release app bundle and package it into a drag-and-drop DMG:

```sh
tools/build-dmg.sh
```

The default output is:

```text
target/dmg/CrossPuck-0.4.0.dmg
```

The DMG opens in a conventional drag-and-drop Finder layout: a large
`CrossPuck.app` icon on the left, an arrow in the middle, and an `Applications`
symlink on the right. The mounted DMG volume uses the app's `CrossPuck.icns`
icon. License files are embedded in the app bundle under `Contents/Resources`.
For GitHub Actions release jobs, codesign the app first and then pass the signed
app bundle to the DMG builder:

```sh
tools/build-dmg.sh --app target/release/CrossPuck.app
```

The script supports Developer ID signing and final-DMG notarization/stapling:

```sh
tools/build-dmg.sh \
  --app-sign-identity "$CROSSPUCK_APP_SIGN_IDENTITY" \
  --dmg-sign-identity "$CROSSPUCK_APP_SIGN_IDENTITY" \
  --notarize \
  --notary-keychain-profile CrossPuck
```

Create the `CrossPuck` notarytool profile beforehand using Apple's
`xcrun notarytool store-credentials`. Sparkle helpers are signed inside-out before
the containing framework and main app. Do not re-sign or staple a DMG after its
Sparkle signature has been generated: either action changes the bytes users
download and invalidates that update signature.

## Sparkle App Updates And GitHub Releases

CrossPuck uses Sparkle 2.10.0 to install whole-app updates from the existing
GitHub Release DMGs. Automatic checks are a persisted menu option, off by
default; background checks show an `Update Available...` menu reminder and
installation still needs user approval. The app runs on macOS 12 or
later. Updating the app does not replace a guest DLL already copied into a
CrossOver bottle; quit Steam and use the driver's install/update menu action
separately.

The stable feed URL embedded in public builds is:

```text
https://github.com/scryner/crosspuck/releases/latest/download/appcast.xml
```

Each feed item points to a version-specific asset URL, for example
`https://github.com/scryner/crosspuck/releases/download/v0.4.0/CrossPuck-0.4.0.dmg`.
This keeps an older feed item tied to the exact DMG bytes its EdDSA signature
covers. Publish `appcast.xml` alongside every stable DMG release so GitHub's
`latest` redirect always leads to a feed. Do not mark a prerelease as latest.
Older apps without Sparkle need one manual DMG installation of a Sparkle-enabled
version before in-app updates become available.

### Update Signing Key

Fetch the pinned distribution and initialize the app's update-signing key once:

```sh
tools/setup-sparkle.sh
build/sparkle-2.10.0/bin/generate_keys --account com.github.scryner.crosspuck
```

`generate_keys` keeps the private key in the login Keychain and prints the public
key. If that account already exists, it reuses the existing key. Maintainers must
retain the same key used by published CrossPuck apps. Put its public key in the
app's `SUPublicEDKey`, or supply it for a build:

```sh
export CROSSPUCK_SPARKLE_PUBLIC_KEY="$(build/sparkle-2.10.0/bin/generate_keys \
  --account com.github.scryner.crosspuck -p)"
```

The release helper looks up this same account and rejects a key that differs
from the public key inside the DMG. It never creates a key. To transfer a key to
another signing Mac or an unlocked CI keychain, use Sparkle's documented
`generate_keys -x`/`-f` export/import commands; keep exported private-key files
out of this repository and release assets. Key loss/rotation needs a deliberate
Sparkle migration. See [Sparkle's key setup](https://sparkle-project.org/documentation/#eddsa-ed25519-signatures).

### Build Configuration

The packaging scripts accept these environment variables:

| Variable | Purpose |
| --- | --- |
| `CROSSPUCK_SPARKLE_DIR` | Existing Sparkle distribution directory; default `build/sparkle-2.10.0` |
| `CROSSPUCK_SPARKLE_PUBLIC_KEY` | Genuine base64 Ed25519 public key; otherwise use the source `Info.plist` value |
| `CROSSPUCK_SPARKLE_FEED_URL` | HTTPS feed override for development or a separate release channel |
| `CROSSPUCK_BUILD_VERSION` | Numeric `CFBundleVersion`; defaults to the stable Cargo `x.y.z` version |
| `CROSSPUCK_SPARKLE_KEYCHAIN_ACCOUNT` | Release-signing key account; default `com.github.scryner.crosspuck` |

`CFBundleVersion` must increase for every update, including a rebuilt release
with different bytes. For prereleases or an independently numbered build series,
set `CROSSPUCK_BUILD_VERSION` consistently and keep it strictly increasing. The
stable GitHub publication helper requires its stable feed URL and a tag matching
`CFBundleShortVersionString` (with an optional `v` prefix).

The shared stable feed accepts Apple silicon or universal app bundles. An
Apple-silicon-only build gets Sparkle's `arm64` hardware requirement, so Intel
users will not be offered it. An Intel-only bundle needs a separate feed; it
cannot replace a native Apple silicon bundle through this shared feed. The
packaging scripts preserve the framework's symlinks and executable permissions.

### Prepare And Publish

Finish Developer ID signing, notarization and stapling first. Then prepare
reviewable assets locally:

```sh
tools/publish-release.sh \
  --dmg target/dmg/CrossPuck-0.4.0.dmg \
  --tag v0.4.0 \
  --notes-file /path/to/release-notes.md \
  --dry-run
```

Preparation is the default even without `--dry-run`. It verifies Developer ID
signatures, the DMG's stapled ticket and Gatekeeper assessment, mounts the DMG
read-only, and inspects the actual bundled app's version, public key and binary
architectures. It also checks Sparkle's helper layout and that nested code uses
the app's signing team. It signs the finalized DMG with Sparkle's `sign_update`, verifies
that signature, and writes:

```text
target/release-assets/v0.4.0/CrossPuck-0.4.0.dmg
target/release-assets/v0.4.0/CrossPuck-0.4.0.dmg.sha256
target/release-assets/v0.4.0/appcast.xml
target/release-assets/v0.4.0/release.json
```

The feed includes the real `CFBundleVersion`, display version, minimum macOS
version, optional Markdown notes, architecture requirement and genuine EdDSA
signature. Up to 20 previous feed items are retained. The helper downloads the
current stable feed to prevent a build downgrade or reuse of a published build
number with different bytes. A 404 is expected for the first release with a feed;
other download failures stop preparation. For offline preparation, pass
`--previous-appcast /path/to/current-appcast.xml`; only the very first feed can
use `--new-feed`. These offline options are not accepted with `--publish`.

After reviewing the generated files, create a draft GitHub release for the
already-pushed version tag, or use an existing stable release:

```sh
gh release create v0.4.0 --repo scryner/crosspuck \
  --verify-tag --draft --title "CrossPuck 0.4.0" \
  --notes-file /path/to/release-notes.md

tools/publish-release.sh \
  --dmg target/dmg/CrossPuck-0.4.0.dmg \
  --tag v0.4.0 \
  --notes-file /path/to/release-notes.md \
  --publish
```

`--publish` requires authenticated GitHub CLI access and an existing stable
release. It uploads the DMG first, downloads it back to verify its SHA-256, then
uploads the checksum and feed. Finally it publishes the draft and marks the
release latest. An existing DMG is never overwritten: different bytes require a
new version and tag. This ordering keeps a feed from advertising an unavailable
asset. `release.json` is a local audit artifact and is not uploaded.

These tools use a full DMG update, without binary deltas or a separate update
server. The appcast is generated with Python's standard library using Sparkle's
[documented feed format](https://sparkle-project.org/documentation/publishing/)
and actual signatures from the official Sparkle tools.

### Update Validation

Run the isolated packaging and synthetic appcast/publish-order checks:

```sh
tools/test-sparkle-packaging.sh
tools/test-sparkle-integration.sh
tools/test-sparkle-feed.sh
python3 -m unittest discover -s tools/tests -v
```

The integration check uses the real Rust/AppKit menu and Sparkle framework in
a disposable app, without starting the HID host or changing production
preferences. The feed check exercises Sparkle's newer/same-version selection
over loopback and verifies that it never downloads a DMG. Its fixture signature
has a synthetic encoding shape; it does not verify archive signatures.

These checks do not prove an installed update. Before publishing the first
updater release, separately exercise a genuine lower-build signed/notarized app
against a higher-build signed/notarized DMG and a test HTTPS feed. Verify manual
check, automatic-check opt-in persistence, installation approval, host shutdown,
whole-bundle replacement and restart, and then the bottle driver's separate
update action. The initial rollout requires a manual DMG install.

## Logging

Host app logs use macOS Unified Logging.

```sh
open -a CrossPuck --args --log-level debug
open -a CrossPuck --args --override-log-level --log-level debug
CROSSPUCK_LOG_LEVEL=debug CrossPuck
```

Supported host levels are `off`, `error`, `warn`, `info`, `debug`, and `trace`.
The default is `info`.

Guest driver logs are written to `crosspuck-driver.log` next to Steam. The
guest default level is `info`. To override guest severity for a host session,
start the host with `--override-log-level --log-level <level>`.

- `info`: attach and bridge connection state.
- `error`: hook/bridge/virtual HID failures.
- `debug`: hook, discovery, and API-level diagnostic logs.
- `trace`: payload traces when enabled by host-owned diagnostic configuration.

## Smoke Test

The detailed CrossOver smoke procedure is documented here:

- `crates/crosspuck-driver/docs/crossover-smoke.md`
- `crates/crosspuck-driver/docs/crossover-smoke-ko.md`

After exercising the Steam UI, run:

```sh
tools/smoke-check.sh --bottle Steam
```

Warnings from this script indicate missing required smoke markers. Optional INFO
markers are expected to be absent unless the corresponding Steam UI path was
exercised or debug/trace logging was enabled.

## Development Checks

Run the main workspace checks:

```sh
cargo fmt --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Check the Windows guest target explicitly:

```sh
cargo check -p crosspuck-driver --target x86_64-pc-windows-gnu
cargo clippy -p crosspuck-driver --target x86_64-pc-windows-gnu -- -D warnings
```

When the MSVC target is available:

```sh
cargo check -p crosspuck-driver --target x86_64-pc-windows-msvc
cargo clippy -p crosspuck-driver --target x86_64-pc-windows-msvc -- -D warnings
```

## Host App Memory Profiling

Run the macOS host app through `cargo run` with the profiling feature and app
probe enabled:

```sh
tools/profile-app.sh
```

By default this writes a capture under `captures/app-profile/YYYYMMDD-HHMMSS`,
samples RSS and probe counters every 5 seconds, records `vmmap -summary` every
30 seconds, and writes a 60-second pprof flamegraph.

Useful options:

```sh
tools/profile-app.sh --dir /tmp/crosspuck-profile --cpu-seconds 120
tools/profile-app.sh --release --interval-ms 1000 --vmmap-interval-ms 10000
tools/profile-app.sh -- --override-log-level --log-level debug
```

After reproducing idle or touchpad-input scenarios, summarize the capture:

```sh
tools/profile-app-summary.sh --dir /tmp/crosspuck-profile
tools/profile-app-summary.sh --dir /tmp/crosspuck-profile --leaks
```

The summary reports RSS deltas, post-warmup deltas, CrossPuck probe counters,
the latest captured `vmmap` line, any generated CPU flamegraphs, and live
`ps`/`vmmap` data if the process is still running.

## macOS HID Reference Probe

The macOS HID reference probe is a host-side reference tracer for the native
macOS Steam client. The shell entrypoints are in `tools/`, while the interposer
source remains under `tools/macos_hid_probe`. It interposes IOKit HID report
calls and logs real feature/output report traffic plus input report callbacks
for the Valve puck (`VID=0x28DE`, `PID=0x1304` by default).

Build:

```sh
tools/build-macos-hid-probe.sh
```

Run native Steam with the probe:

```sh
tools/launch-steam-with-probe.sh
```

Run the feedback scenario capture for protocol design:

```sh
tools/capture-feedback-scenarios.sh
```

This launches native Steam with the probe, writes JSONL markers for these
manual windows, and runs the CLI analyzer at the end:

- left touchpad haptic, 5 seconds
- right touchpad haptic, 5 seconds
- Steam controller test "Ping", 5 seconds

Do not run `crosspuck-host`, `cargo run --bin crosspuck-host`, or any other
standalone HID reader during this capture. Those tools open the puck directly
and can prevent native Steam from recognizing it. The feedback capture must be
probe-only: the dylib observes HID calls inside the Steam process and does not
open the device on its own.

The launch scripts abort if native Steam or `crosspuck-host` is already
running. Quit Steam fully before starting a capture. If you intentionally need
to attach to an already-running Steam process tree, set
`CROSSPUCK_ALLOW_RUNNING_STEAM=1`, but that mode is not suitable for reference
captures.

Default log:

```text
/tmp/crosspuck-host-hid.log
```

Steam stdout/stderr is also captured. By default it is written next to the HID
probe log:

```text
/tmp/crosspuck-host-hid.stdout.log
```

Useful environment variables:

```sh
CROSSPUCK_HOST_HID_LOG=/tmp/crosspuck-host-hid.log
CROSSPUCK_STEAM_STDOUT_LOG=/tmp/crosspuck-steam-stdout.log
CROSSPUCK_HOST_HID_VID=0x28DE
CROSSPUCK_HOST_HID_PID=0x1304
CROSSPUCK_HOST_HID_MAX_BYTES=256
CROSSPUCK_HOST_HID_LOG_ALL=1
CROSSPUCK_HOST_HID_JSONL=1
CROSSPUCK_HOST_HID_LOG_LOAD=1
CROSSPUCK_HOST_HID_LOG_INPUT=0
CROSSPUCK_HOST_HID_LOG_GET=1
CROSSPUCK_HOST_HID_LOG_VALUE=1
STEAM_OSX="$HOME/Library/Application Support/Steam/Steam.AppBundle/Steam/Contents/MacOS/steam_osx"
```

If `STEAM_OSX` is not set, the scripts prefer the installed `Steam.AppBundle`
client binary under `~/Library/Application Support/Steam` and fall back to
`/Applications/Steam.app/...`.

The launchers run Steam with its current working directory set to the selected
binary's `Contents/MacOS` directory. Probe load logging is disabled by default
(`CROSSPUCK_HOST_HID_LOG_LOAD=0`) so the injected library does not do file I/O
or symbol lookup until Steam actually calls one of the interposed HID APIs. Set
`CROSSPUCK_HOST_HID_LOG_LOAD=1` only when checking whether the dylib is loaded
into a process.

The key sequence to compare against the CrossOver guest is usually:

```text
SET type=feature report_id=...
GET request type=feature report_id=...
GET result type=feature report_id=... bytes=...
REGISTER input_report_callback ...
INPUT callback type=input report_id=... bytes=...
```

For feedback analysis, focus on JSONL records with:

```text
type=hid_probe
direction=host_to_device
phase=request
event=set_report | set_report_callback | set_value*
```

Analyze an existing JSONL capture:

```sh
cargo run -p crosspuck-cli -- --analyze-hid-probe captures/native_feedback_YYYYMMDD-HHMMSS.jsonl
```

For the controller-recognition failure, capture from native Steam startup
through the point where the UI shows the Steam Controller as connected. The
important comparison points are native `02 B4`, `01 83`, `01 F2`, `01 AE`, and
the first input reports that lead to `WIRELESS SYSTEM DEBUG` / `Got bond` in
Steam's controller log.
