#!/usr/bin/env python3
"""Prepare a signed Sparkle appcast for CrossPuck's existing GitHub DMG releases.

Only --publish writes to GitHub. Metadata is always read from the app inside the
final, signed and stapled DMG, never from a potentially different build directory.
"""

import argparse
import base64
import copy
import datetime
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parent.parent
SPARKLE = "http://www.andymatuschak.org/xml-namespaces/sparkle"
ET.register_namespace("sparkle", SPARKLE)
APP_ID = "com.github.scryner.crosspuck"
DEFAULT_REPO = "scryner/crosspuck"


class ReleaseError(Exception):
    """A release cannot be safely prepared or published."""


def sparkle(name):
    return "{" + SPARKLE + "}" + name


def numeric_version(value):
    """CrossPuck builds use one to three numeric components, padded for comparison."""
    if not isinstance(value, str) or not re.fullmatch(r"[0-9]+(?:\.[0-9]+){0,2}", value):
        raise ReleaseError(f"Invalid numeric build/system version: {value!r}")
    components = tuple(int(part) for part in value.split("."))
    return components + (0,) * (3 - len(components))


def encoded_bytes(value, count, label):
    try:
        decoded = base64.b64decode(value, validate=True)
    except (ValueError, TypeError) as error:
        raise ReleaseError(f"{label} is not valid base64") from error
    if len(decoded) != count:
        raise ReleaseError(f"{label} must decode to {count} bytes")
    return decoded


def github_urls(repo, tag, filename):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ReleaseError("--repo must have the form OWNER/REPOSITORY")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", tag):
        raise ReleaseError("--tag must contain only letters, numbers, dots, underscores and hyphens")
    base = f"https://github.com/{repo}/releases"
    return {
        "feed": f"{base}/latest/download/appcast.xml",
        "release": f"{base}/tag/{urllib.parse.quote(tag, safe='')}",
        "dmg": f"{base}/download/{urllib.parse.quote(tag, safe='')}/{urllib.parse.quote(filename, safe='')}",
    }


def bundle_metadata(plist, architectures, expected_feed, tag):
    if plist.get("CFBundleIdentifier") != APP_ID:
        raise ReleaseError(f"The DMG must contain the {APP_ID} app")
    version = plist.get("CFBundleVersion")
    numeric_version(version)
    short_version = plist.get("CFBundleShortVersionString")
    if not isinstance(short_version, str) or tag.removeprefix("v") != short_version:
        raise ReleaseError("The release tag must match CFBundleShortVersionString (with an optional v prefix)")
    public_key = plist.get("SUPublicEDKey")
    encoded_bytes(public_key, 32, "The app's SUPublicEDKey")
    if plist.get("SUFeedURL") != expected_feed:
        raise ReleaseError(f"The app's SUFeedURL must be {expected_feed} for this GitHub release workflow")
    minimum = numeric_version(plist.get("LSMinimumSystemVersion", "12.0"))
    if minimum < (12, 0, 0):
        raise ReleaseError("Sparkle 2.10.0 releases must require macOS 12 or later")
    architecture_set = set(architectures)
    if architecture_set not in ({"arm64"}, {"arm64", "x86_64"}):
        raise ReleaseError("The shared stable feed requires an arm64 or universal app; Intel-only builds need a separate feed")
    return {
        "version": version,
        "short_version": short_version,
        "public_key": public_key,
        "minimum_system": ".".join(str(part) for part in minimum),
        "architectures": sorted(architecture_set),
    }


def parse_previous_appcast(data, repo):
    if not data:
        return []
    if len(data) > 4 * 1024 * 1024:
        raise ReleaseError("Previous appcast is larger than 4 MiB")
    # A feed is data, not an XML document with external or internal entity definitions.
    if b"<!DOCTYPE" in data.upper() or b"<!ENTITY" in data.upper():
        raise ReleaseError("Previous appcast must not contain DTD/entity declarations")
    try:
        root = ET.fromstring(data)
    except ET.ParseError as error:
        raise ReleaseError(f"Invalid previous appcast XML: {error}") from error
    channel = root.find("channel")
    if root.tag != "rss" or channel is None:
        raise ReleaseError("Previous appcast must contain an RSS channel")
    items = []
    for item in channel.findall("item"):
        enclosure = item.find("enclosure")
        if enclosure is None:
            raise ReleaseError("Previous appcast item is missing a downloadable enclosure")
        version = item.findtext(sparkle("version")) or enclosure.get(sparkle("version"))
        numeric_version(version)
        url = enclosure.get("url", "")
        prefix = f"https://github.com/{repo}/releases/download/"
        if not url.startswith(prefix):
            raise ReleaseError("Previous appcast enclosures must use version-specific assets from this repository")
        encoded_bytes(enclosure.get(sparkle("edSignature")), 64, "Previous enclosure EdDSA signature")
        if not enclosure.get("length", "").isdigit() or int(enclosure.get("length")) <= 0:
            raise ReleaseError("Previous enclosure must have a positive byte length")
        items.append(copy.deepcopy(item))
    return items


def item_version(item):
    enclosure = item.find("enclosure")
    return item.findtext(sparkle("version")) or enclosure.get(sparkle("version"))


def make_appcast(metadata, urls, signature, length, previous, notes="", published_at=None):
    encoded_bytes(signature, 64, "EdDSA signature")
    if length <= 0:
        raise ReleaseError("The DMG must not be empty")
    current = numeric_version(metadata["version"])
    retained = []
    for old_item in previous:
        old_version = numeric_version(item_version(old_item))
        if old_version > current:
            raise ReleaseError("CFBundleVersion must be newer than all previously published builds")
        if old_version == current:
            old_enclosure = old_item.find("enclosure")
            if (old_enclosure.get("url") != urls["dmg"]
                    or old_enclosure.get(sparkle("edSignature")) != signature
                    or old_enclosure.get("length") != str(length)):
                raise ReleaseError("A published CFBundleVersion cannot be reused for different DMG bytes or URLs")
            # Idempotent retries preserve the original publication date and notes.
            retained.append(old_item)
        else:
            retained.append(old_item)
    if not any(numeric_version(item_version(item)) == current for item in retained):
        item = ET.Element("item")
        ET.SubElement(item, "title").text = f"CrossPuck {metadata['short_version']}"
        ET.SubElement(item, "link").text = urls["release"]
        ET.SubElement(item, sparkle("version")).text = metadata["version"]
        ET.SubElement(item, sparkle("shortVersionString")).text = metadata["short_version"]
        ET.SubElement(item, sparkle("minimumSystemVersion")).text = metadata["minimum_system"]
        if metadata["architectures"] == ["arm64"]:
            ET.SubElement(item, sparkle("hardwareRequirements")).text = "arm64"
        now = published_at or datetime.datetime.now(datetime.timezone.utc)
        ET.SubElement(item, "pubDate").text = now.strftime("%a, %d %b %Y %H:%M:%S %z")
        if notes:
            ET.SubElement(item, "description", {sparkle("format"): "markdown"}).text = notes
        ET.SubElement(item, "enclosure", {
            "url": urls["dmg"],
            "length": str(length),
            "type": "application/octet-stream",
            sparkle("edSignature"): signature,
        })
        retained.append(item)
    retained.sort(key=lambda item: numeric_version(item_version(item)), reverse=True)
    rss = ET.Element("rss", {"version": "2.0"})
    channel = ET.SubElement(rss, "channel")
    ET.SubElement(channel, "title").text = "CrossPuck Updates"
    ET.SubElement(channel, "link").text = urls["release"].split("/releases/")[0]
    ET.SubElement(channel, "description").text = "Signed CrossPuck macOS app updates"
    ET.SubElement(channel, "language").text = "en"
    for item in retained[:20]:
        channel.append(item)
    ET.indent(rss, space="  ")
    return ET.tostring(rss, encoding="utf-8", xml_declaration=True) + b"\n"


def run(command, capture=False):
    try:
        result = subprocess.run(command, check=True, text=True, stdout=subprocess.PIPE if capture else None)
    except FileNotFoundError as error:
        raise ReleaseError(f"Required command not found: {command[0]}") from error
    except subprocess.CalledProcessError as error:
        raise ReleaseError(f"Command failed ({error.returncode}): {command[0]}") from error
    return result.stdout.strip() if capture else None


def signature_details(path):
    try:
        result = subprocess.run(["codesign", "--display", "--verbose=4", str(path)], check=True,
                                capture_output=True, text=True)
    except subprocess.CalledProcessError as error:
        raise ReleaseError(f"Cannot inspect code signature: {path}") from error
    return result.stderr


def require_developer_id(path):
    run(["codesign", "--verify", "--strict", "--deep", str(path)])
    details = signature_details(path)
    if "Authority=Developer ID Application:" not in details:
        raise ReleaseError(f"Developer ID Application signature required: {path}")
    return details


def verify_sparkle_layout(app, app_signature):
    framework = app / "Contents/Frameworks/Sparkle.framework"
    for relative in ("Versions/Current", "Sparkle", "Resources"):
        if not (framework / relative).is_symlink():
            raise ReleaseError(f"Sparkle framework symlink is missing: {relative}")
    nested = [
        ("XPCServices/Installer.xpc", "Contents/MacOS/Installer"),
        ("XPCServices/Downloader.xpc", "Contents/MacOS/Downloader"),
        ("Autoupdate", ""), ("Updater.app", "Contents/MacOS/Updater"),
    ]
    app_team_match = re.search(r"^TeamIdentifier=(.+)$", app_signature, re.MULTILINE)
    if app_team_match is None or app_team_match.group(1) == "not set":
        raise ReleaseError("The app must have a Developer ID TeamIdentifier")
    app_team = app_team_match.group(1)
    for relative, executable in nested + [("", "Sparkle")]:
        code = framework / "Versions/B" / relative
        if not os.access(code / executable if executable else code, os.X_OK):
            raise ReleaseError(f"Sparkle executable/helper is missing: {code}")
        details = signature_details(framework if not relative else code)
        team_match = re.search(r"^TeamIdentifier=(.+)$", details, re.MULTILINE)
        if team_match is None or team_match.group(1) != app_team:
            raise ReleaseError(f"Sparkle nested code must be signed by the app's team: {code}")


def inspect_dmg(dmg, urls, tag):
    require_developer_id(dmg)
    run(["xcrun", "stapler", "validate", str(dmg)])
    run(["spctl", "--assess", "--type", "open", "--context", "context:primary-signature", str(dmg)])
    with tempfile.TemporaryDirectory(prefix="crosspuck-release-mount-") as directory:
        mount = Path(directory) / "volume"
        mounted = False
        try:
            run(["hdiutil", "attach", "-readonly", "-nobrowse", "-noautoopen", "-mountpoint", str(mount), str(dmg)])
            mounted = True
            apps = list(mount.glob("*.app"))
            if len(apps) != 1 or apps[0].name != "CrossPuck.app":
                raise ReleaseError("The DMG must contain exactly one top-level CrossPuck.app")
            app = apps[0]
            app_signature = require_developer_id(app)
            verify_sparkle_layout(app, app_signature)
            run(["spctl", "--assess", "--type", "execute", str(app)])
            with (app / "Contents/Info.plist").open("rb") as file:
                plist = plistlib.load(file)
            executable = plist.get("CFBundleExecutable")
            if not isinstance(executable, str) or Path(executable).name != executable:
                raise ReleaseError("Invalid CFBundleExecutable in the DMG")
            architectures = run(["lipo", "-archs", str(app / "Contents/MacOS" / executable)], capture=True).split()
            return bundle_metadata(plist, architectures, urls["feed"], tag)
        finally:
            if mounted:
                run(["hdiutil", "detach", str(mount)])


def read_previous(args, feed_url):
    if args.previous_appcast:
        return args.previous_appcast.read_bytes()
    if args.new_feed:
        return b""
    try:
        with urllib.request.urlopen(feed_url, timeout=30) as response:
            return response.read(4 * 1024 * 1024 + 1)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return b""  # First release with in-app updates.
        raise ReleaseError(f"Could not retrieve the current appcast: HTTP {error.code}") from error
    except (urllib.error.URLError, TimeoutError) as error:
        raise ReleaseError("Could not retrieve the current appcast; use --previous-appcast for offline preparation") from error


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def publish(args, prepared_dmg, appcast, checksum, expected_hash):
    # Operate on an existing stable release. Drafts become public only after all
    # assets are ready; release creation remains an explicit maintainer action.
    release = json.loads(run(["gh", "release", "view", args.tag, "--repo", args.repo,
                              "--json", "isDraft,isPrerelease,assets"], capture=True))
    if release["isPrerelease"]:
        raise ReleaseError("The stable appcast must not be published to a prerelease")
    asset_names = {asset["name"] for asset in release["assets"]}
    if prepared_dmg.name not in asset_names:
        run(["gh", "release", "upload", args.tag, str(prepared_dmg), "--repo", args.repo])
    # Always confirm the remote DMG bytes before making an appcast visible. DMGs
    # are immutable: an existing name is never clobbered with another binary.
    with tempfile.TemporaryDirectory(prefix="crosspuck-release-download-") as directory:
        run(["gh", "release", "download", args.tag, "--repo", args.repo,
             "--pattern", prepared_dmg.name, "--dir", directory])
        if sha256(Path(directory) / prepared_dmg.name) != expected_hash:
            raise ReleaseError("The release already contains different DMG bytes; use a new version/tag")
    run(["gh", "release", "upload", args.tag, str(checksum), "--repo", args.repo, "--clobber"])
    run(["gh", "release", "upload", args.tag, str(appcast), "--repo", args.repo, "--clobber"])
    run(["gh", "release", "edit", args.tag, "--repo", args.repo, "--draft=false", "--latest"])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dmg", type=Path, required=True, help="Final Developer ID signed, notarized and stapled DMG")
    parser.add_argument("--tag", required=True, help="Existing GitHub release tag matching the app version")
    parser.add_argument("--repo", default=DEFAULT_REPO)
    parser.add_argument("--sparkle-dir", type=Path, default=Path(os.environ.get("CROSSPUCK_SPARKLE_DIR", ROOT / "build/sparkle-2.10.0")))
    parser.add_argument("--keychain-account", default=os.environ.get("CROSSPUCK_SPARKLE_KEYCHAIN_ACCOUNT", APP_ID))
    parser.add_argument("--notes-file", type=Path, help="Markdown release notes embedded in the appcast")
    parser.add_argument("--previous-appcast", type=Path, help="Preserve this feed's items; otherwise download the latest feed")
    parser.add_argument("--new-feed", action="store_true", help="Prepare the first feed offline (never use to reset published history)")
    parser.add_argument("--output-dir", type=Path, help="Defaults to target/release-assets/TAG")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--dry-run", action="store_true", help="Prepare and validate assets without GitHub writes (the default)")
    mode.add_argument("--publish", action="store_true", help="Upload to an existing stable GitHub release, then publish/promote it")
    args = parser.parse_args(argv)
    if args.previous_appcast and args.new_feed:
        raise ReleaseError("Use only one of --previous-appcast and --new-feed")
    if args.publish and args.new_feed:
        raise ReleaseError("--new-feed is for offline preparation; publishing must retrieve current history")
    if args.publish and args.previous_appcast:
        raise ReleaseError("--previous-appcast is for offline preparation; publishing must retrieve current history")
    dmg = args.dmg.resolve()
    if not dmg.is_file() or dmg.suffix.lower() != ".dmg":
        raise ReleaseError("--dmg must point to an existing .dmg file")
    urls = github_urls(args.repo, args.tag, dmg.name)
    metadata = inspect_dmg(dmg, urls, args.tag)
    tools = args.sparkle_dir.resolve() / "bin"
    for name in ("generate_keys", "sign_update"):
        if not os.access(tools / name, os.X_OK):
            raise ReleaseError(f"Missing Sparkle tool: {tools / name}; run tools/setup-sparkle.sh")
    key = run([str(tools / "generate_keys"), "--account", args.keychain_account, "-p"], capture=True)
    if encoded_bytes(key, 32, "Keychain public key") != encoded_bytes(metadata["public_key"], 32, "App public key"):
        raise ReleaseError("The selected signing key does not match the DMG app's SUPublicEDKey")
    previous = parse_previous_appcast(read_previous(args, urls["feed"]), args.repo)
    notes = args.notes_file.read_text(encoding="utf-8") if args.notes_file else ""
    output = (args.output_dir or ROOT / "target/release-assets" / args.tag).resolve()
    output.mkdir(parents=True, exist_ok=True)
    prepared_dmg = output / dmg.name
    if prepared_dmg != dmg:
        shutil.copy2(dmg, prepared_dmg)
    # Signing happens after all Apple signing/notarization/stapling, and after
    # copying the exact asset that will be uploaded. No later mutation is allowed.
    signature = run([str(tools / "sign_update"), "--account", args.keychain_account, "-p", str(prepared_dmg)], capture=True)
    encoded_bytes(signature, 64, "sign_update output")
    run([str(tools / "sign_update"), "--account", args.keychain_account, "--verify", str(prepared_dmg), signature])
    content = make_appcast(metadata, urls, signature, prepared_dmg.stat().st_size, previous, notes)
    appcast = output / "appcast.xml"
    appcast.write_bytes(content)
    digest = sha256(prepared_dmg)
    checksum = output / (prepared_dmg.name + ".sha256")
    checksum.write_text(f"{digest}  {prepared_dmg.name}\n", encoding="utf-8")
    manifest = dict(metadata, tag=args.tag, repository=args.repo, urls=urls,
                    dmg_file=prepared_dmg.name, dmg_bytes=prepared_dmg.stat().st_size,
                    dmg_sha256=digest, ed_signature=signature)
    (output / "release.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Prepared release assets: {output}")
    print(f"Build {metadata['version']}, macOS {metadata['minimum_system']}+, {', '.join(metadata['architectures'])}")
    print(f"Update feed: {urls['feed']}")
    if args.publish:
        publish(args, prepared_dmg, appcast, checksum, digest)
        print(f"Published: {urls['release']}")
    else:
        print("No GitHub assets were changed. Review appcast.xml and release.json before --publish.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ReleaseError, OSError, plistlib.InvalidFileException, json.JSONDecodeError) as error:
        print(f"Release preparation failed: {error}", file=sys.stderr)
        sys.exit(1)
