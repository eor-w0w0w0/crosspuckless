#!/usr/bin/env bash
set -euo pipefail

# Actual Sparkle feed parsing/version selection using disposable native apps.
# No product launch, HID/Steam service, Keychain access, download, or install.
# The fixture signature has only the required shape; this does not validate an
# archive signature. Production update signatures are tested separately.
root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
sparkle_dir="$("$root_dir/tools/setup-sparkle.sh" --verify-only)"
fixture="$(mktemp -d "${TMPDIR:-/tmp}/crosspuck-feed-test.XXXXXX")"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  # Failed probes also clean their unique preferences; never use product IDs.
  for mode in newer same; do
    for suffix in app preferences; do
      /usr/bin/defaults delete "com.github.scryner.crosspuck.feed-smoke.$fixture_id.$mode.$suffix" >/dev/null 2>&1 || true
    done
  done
  rm -rf "$fixture"
}
fixture_id="$(/usr/bin/uuidgen | tr '[:upper:]' '[:lower:]')"
trap cleanup EXIT

cat > "$fixture/server.py" <<'PY'
import base64
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path
import sys

fixture = Path(sys.argv[1])
signature = base64.b64encode(bytes(64)).decode()

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        with (fixture / 'requests.log').open('a') as log:
            log.write(self.path + '\n')
        mode = self.path.split('?', 1)[0]
        if mode not in ('/newer.xml', '/same.xml'):
            self.send_error(500, 'Unexpected request: informational probes must not download')
            return
        version = '2.0.0' if mode == '/newer.xml' else '1.0.0'
        archive = 'CrossPuck-newer.dmg' if mode == '/newer.xml' else 'CrossPuck-same.dmg'
        document = f'''<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel><title>CrossPuck isolated feed fixture</title><item>
    <title>CrossPuck {version}</title>
    <sparkle:version>{version}</sparkle:version>
    <sparkle:shortVersionString>{version}</sparkle:shortVersionString>
    <sparkle:minimumSystemVersion>12.0</sparkle:minimumSystemVersion>
    <sparkle:hardwareRequirements>arm64</sparkle:hardwareRequirements>
    <enclosure url="http://localhost:{self.server.server_port}/{archive}"
      length="1" type="application/octet-stream" sparkle:os="macos"
      sparkle:installationType="application" sparkle:edSignature="{signature}" />
  </item></channel>
</rss>'''.encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/rss+xml')
        self.send_header('Content-Length', str(len(document)))
        self.end_headers()
        self.wfile.write(document)

    def log_message(self, *args):
        pass

server = HTTPServer(('127.0.0.1', 0), Handler)
(fixture / 'port').write_text(str(server.server_port))
server.serve_forever()
PY
python3 "$fixture/server.py" "$fixture" > "$fixture/server.log" 2>&1 &
server_pid="$!"
for _ in {1..50}; do
  [[ -s "$fixture/port" ]] && break
  if ! kill -0 "$server_pid" 2>/dev/null; then
    cat "$fixture/server.log" >&2
    echo "FAIL: loopback fixture server could not start" >&2
    exit 1
  fi
  sleep 0.1
done
[[ -s "$fixture/port" ]] || { echo "FAIL: loopback server start timed out" >&2; exit 1; }
port="$(cat "$fixture/port")"

swiftc -parse-as-library -swift-version 5 \
  -module-cache-path "$fixture/module-cache" \
  -F "$sparkle_dir" -framework Sparkle -framework AppKit \
  -Xlinker -rpath -Xlinker '@executable_path/../Frameworks' \
  "$root_dir/tools/tests/sparkle_feed_smoke.swift" -o "$fixture/feed-smoke"
for mode in newer same; do
  app="$fixture/$mode.app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks"
  cp "$fixture/feed-smoke" "$app/Contents/MacOS/feed-smoke"
  ditto "$sparkle_dir/Sparkle.framework" "$app/Contents/Frameworks/Sparkle.framework"
  python3 - "$app/Contents/Info.plist" "$fixture_id" "$mode" "$port" <<'PY'
from pathlib import Path
import plistlib
import sys

path, fixture_id, mode, port = sys.argv[1:]
identifier = f'com.github.scryner.crosspuck.feed-smoke.{fixture_id}.{mode}.app'
domain = f'com.github.scryner.crosspuck.feed-smoke.{fixture_id}.{mode}.preferences'
settings = {
    'CFBundleExecutable': 'feed-smoke',
    'CFBundleIdentifier': identifier,
    'CFBundleName': 'CrossPuck Feed Smoke',
    'CFBundlePackageType': 'APPL',
    'CFBundleVersion': '1.0.0',
    'CFBundleShortVersionString': '1.0.0',
    'LSMinimumSystemVersion': '12.0',
    'LSUIElement': True,
    'SUDefaultsDomain': domain,
    'SUFeedURL': f'http://localhost:{port}/{mode}.xml',
    # Fixture-only public key with a valid Ed25519 encoding shape.
    'SUPublicEDKey': 'AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=',
    'SUEnableAutomaticChecks': False,
    'SUAutomaticallyUpdate': False,
    'SUAllowsAutomaticUpdates': False,
    'SUEnableSystemProfiling': False,
    'SUShowReleaseNotes': False,
    # Local HTTP is permitted only in this disposable fixture Info.plist.
    'NSAppTransportSecurity': {'NSAllowsLocalNetworking': True},
}
Path(path).write_bytes(plistlib.dumps(settings))
PY
  python3 - "$app/Contents/MacOS/feed-smoke" "$mode" <<'PY'
import subprocess
import sys

try:
    result = subprocess.run(sys.argv[1:], timeout=15)
except subprocess.TimeoutExpired:
    raise SystemExit('FAIL: native informational probe exceeded the 15 second limit')
raise SystemExit(result.returncode)
PY
done
python3 - "$fixture/requests.log" <<'PY'
from pathlib import Path
import sys

requests = Path(sys.argv[1]).read_text().splitlines()
paths = [request.split('?', 1)[0] for request in requests]
assert paths == ['/newer.xml', '/same.xml'], requests
print('PASS: only the two appcasts were fetched; no DMG, release notes, download, or install')
PY
echo "Sparkle informational-feed smoke checks passed (detection only, not archive signature validation)."
