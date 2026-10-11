"""Synthetic feed/safety tests; these do not validate an installed Sparkle update."""

import base64
import datetime
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET


SPEC = importlib.util.spec_from_file_location("appcast", Path(__file__).parents[1] / "generate-appcast.py")
appcast = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(appcast)
KEY = base64.b64encode(bytes(range(32))).decode()
SIGNATURE = base64.b64encode(bytes(range(64))).decode()
OTHER_SIGNATURE = base64.b64encode(bytes(reversed(range(64)))).decode()
DATE = datetime.datetime(2026, 10, 11, 1, 2, 3, tzinfo=datetime.timezone.utc)


def metadata(version="0.5.0", architectures=None):
    return {
        "version": version, "short_version": "0.5.0", "public_key": KEY,
        "minimum_system": "12.0.0", "architectures": architectures or ["arm64"],
    }


def urls(tag="v0.5.0"):
    return appcast.github_urls("scryner/crosspuck", tag, f"CrossPuck-{tag[1:]}.dmg")


def bundle():
    return {
        "CFBundleIdentifier": appcast.APP_ID, "CFBundleVersion": "0.5.0",
        "CFBundleShortVersionString": "0.5.0", "SUPublicEDKey": KEY,
        "SUFeedURL": urls()["feed"], "LSMinimumSystemVersion": "12.0",
    }


class AppcastTests(unittest.TestCase):
    def test_reads_actual_build_version_and_system_requirement(self):
        plist = bundle()
        plist["CFBundleVersion"] = "500"
        plist["LSMinimumSystemVersion"] = "14.2"
        result = appcast.bundle_metadata(plist, ["arm64"], urls()["feed"], "v0.5.0")
        self.assertEqual(result["version"], "500")
        self.assertEqual(result["minimum_system"], "14.2.0")

    def test_rejects_wrong_app_key_tag_or_feed(self):
        invalid = {
            "CFBundleIdentifier": "other.app", "SUPublicEDKey": "placeholder",
            "CFBundleShortVersionString": "0.4.0", "SUFeedURL": "https://example.com/feed.xml",
            "LSMinimumSystemVersion": "11.0", "CFBundleVersion": "git-deadbeef",
        }
        for key, value in invalid.items():
            with self.subTest(key=key):
                plist = dict(bundle(), **{key: value})
                with self.assertRaises(appcast.ReleaseError):
                    appcast.bundle_metadata(plist, ["arm64"], urls()["feed"], "v0.5.0")

    def test_intel_only_requires_separate_feed(self):
        with self.assertRaises(appcast.ReleaseError):
            appcast.bundle_metadata(bundle(), ["x86_64"], urls()["feed"], "v0.5.0")

    def test_arm_only_feed_has_hardware_requirement_and_version_specific_url(self):
        xml = appcast.make_appcast(metadata(), urls(), SIGNATURE, 12345, [], published_at=DATE)
        item = ET.fromstring(xml).find("channel/item")
        self.assertEqual(item.findtext(appcast.sparkle("hardwareRequirements")), "arm64")
        self.assertEqual(item.findtext(appcast.sparkle("version")), "0.5.0")
        self.assertEqual(item.findtext("pubDate"), "Sun, 11 Oct 2026 01:02:03 +0000")
        self.assertEqual(item.find("enclosure").get("url"), urls()["dmg"])
        self.assertNotIn("latest", item.find("enclosure").get("url"))

    def test_universal_feed_does_not_exclude_intel(self):
        xml = appcast.make_appcast(metadata(architectures=["arm64", "x86_64"]), urls(), SIGNATURE, 1, [])
        self.assertIsNone(ET.fromstring(xml).find("channel/item/" + appcast.sparkle("hardwareRequirements")))

    def test_markdown_notes_round_trip_without_xml_injection(self):
        notes = "# Fixed <input> & audio\n\n- A \"quote\""
        xml = appcast.make_appcast(metadata(), urls(), SIGNATURE, 1, [], notes)
        description = ET.fromstring(xml).find("channel/item/description")
        self.assertEqual(description.text, notes)
        self.assertEqual(description.get(appcast.sparkle("format")), "markdown")

    def test_retains_previous_items_and_sorts_numeric_versions(self):
        old = appcast.make_appcast(metadata("0.9.0"), urls("v0.4.0"), SIGNATURE, 1, [])
        previous = appcast.parse_previous_appcast(old, "scryner/crosspuck")
        new = appcast.make_appcast(metadata("0.10.0"), urls(), OTHER_SIGNATURE, 2, previous)
        items = ET.fromstring(new).findall("channel/item")
        self.assertEqual([appcast.item_version(item) for item in items], ["0.10.0", "0.9.0"])

    def test_rejects_build_downgrade(self):
        old = appcast.make_appcast(metadata("500"), urls(), SIGNATURE, 1, [])
        previous = appcast.parse_previous_appcast(old, "scryner/crosspuck")
        with self.assertRaises(appcast.ReleaseError):
            appcast.make_appcast(metadata("499"), urls(), SIGNATURE, 1, previous)

    def test_rejects_reusing_published_build_with_other_bytes(self):
        old = appcast.make_appcast(metadata(), urls(), SIGNATURE, 1, [])
        previous = appcast.parse_previous_appcast(old, "scryner/crosspuck")
        with self.assertRaises(appcast.ReleaseError):
            appcast.make_appcast(metadata(), urls(), OTHER_SIGNATURE, 1, previous)

    def test_retry_preserves_existing_item_and_has_no_duplicate(self):
        old = appcast.make_appcast(metadata(), urls(), SIGNATURE, 1, [], "original notes", DATE)
        previous = appcast.parse_previous_appcast(old, "scryner/crosspuck")
        xml = appcast.make_appcast(metadata(), urls(), SIGNATURE, 1, previous, "changed notes")
        items = ET.fromstring(xml).findall("channel/item")
        self.assertEqual(len(items), 1)
        self.assertEqual(items[0].findtext("description"), "original notes")

    def test_invalid_previous_feed_cannot_reset_history(self):
        valid = appcast.make_appcast(metadata(), urls(), SIGNATURE, 1, [])
        invalid = [
            b"<rss />", b"<rss><channel>", b"<!DOCTYPE rss><rss><channel/></rss>",
            valid.replace(SIGNATURE.encode(), b"placeholder"),
            valid.replace(b"https://github.com/scryner/crosspuck/releases/download/", b"http://evil.invalid/"),
            valid.replace(b'length="1"', b'length="0"'),
        ]
        for data in invalid:
            with self.subTest(data=data[:80]):
                with self.assertRaises(appcast.ReleaseError):
                    appcast.parse_previous_appcast(data, "scryner/crosspuck")

    def test_repo_and_tag_cannot_escape_github_url_or_command(self):
        for repo, tag in [("scryner/crosspuck/../other", "v0.5.0"), ("scryner/crosspuck", "--latest"),
                          ("scryner/crosspuck", "v0.5.0\nother"), ("scryner/crosspuck", "release/0.5.0")]:
            with self.assertRaises(appcast.ReleaseError):
                appcast.github_urls(repo, tag, "CrossPuck.dmg")


class PublishOrderTests(unittest.TestCase):
    def test_dmg_uploaded_and_verified_before_feed_and_draft_publication(self):
        calls = []
        release = json.dumps({"isDraft": True, "isPrerelease": False, "assets": []})

        def fake_run(command, capture=False):
            calls.append(command)
            return release if command[1:3] == ["release", "view"] else None

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            args = SimpleNamespace(tag="v0.5.0", repo="scryner/crosspuck")
            with patch.object(appcast, "run", fake_run), patch.object(appcast, "sha256", return_value="hash"):
                appcast.publish(args, root / "CrossPuck-0.5.0.dmg", root / "appcast.xml", root / "CrossPuck.dmg.sha256", "hash")
        operations = [command[2] for command in calls]
        self.assertEqual(operations, ["view", "upload", "download", "upload", "upload", "edit"])
        self.assertNotIn("--clobber", calls[1])
        self.assertEqual(Path(calls[-2][4]).name, "appcast.xml")
        self.assertIn("--draft=false", calls[-1])

    def test_remote_dmg_mismatch_stops_before_feed_upload(self):
        calls = []
        release = json.dumps({"isDraft": False, "isPrerelease": False, "assets": [{"name": "CrossPuck.dmg"}]})

        def fake_run(command, capture=False):
            calls.append(command)
            return release if command[1:3] == ["release", "view"] else None

        args = SimpleNamespace(tag="v0.5.0", repo="scryner/crosspuck")
        with patch.object(appcast, "run", fake_run), patch.object(appcast, "sha256", return_value="different"):
            with self.assertRaises(appcast.ReleaseError):
                appcast.publish(args, Path("CrossPuck.dmg"), Path("appcast.xml"), Path("checksum"), "hash")
        self.assertEqual([command[2] for command in calls], ["view", "download"])

    def test_prerelease_does_not_receive_stable_feed(self):
        release = json.dumps({"isDraft": False, "isPrerelease": True, "assets": []})
        args = SimpleNamespace(tag="v0.5.0", repo="scryner/crosspuck")
        with patch.object(appcast, "run", return_value=release) as mocked:
            with self.assertRaises(appcast.ReleaseError):
                appcast.publish(args, Path("CrossPuck.dmg"), Path("appcast.xml"), Path("checksum"), "hash")
        self.assertEqual(mocked.call_count, 1)


class SparkleLayoutTests(unittest.TestCase):
    @staticmethod
    def layout(root):
        framework = root / "Contents/Frameworks/Sparkle.framework"
        version = framework / "Versions/B"
        executables = ["Sparkle", "Autoupdate", "Updater.app/Contents/MacOS/Updater",
                       "XPCServices/Installer.xpc/Contents/MacOS/Installer",
                       "XPCServices/Downloader.xpc/Contents/MacOS/Downloader"]
        for relative in executables:
            file = version / relative
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("synthetic fixture")
            file.chmod(0o755)
        (version / "Resources").mkdir()
        (framework / "Versions/Current").symlink_to("B")
        (framework / "Sparkle").symlink_to("Versions/Current/Sparkle")
        (framework / "Resources").symlink_to("Versions/Current/Resources")
        return framework

    def test_complete_layout_and_matching_team(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory)
            self.layout(app)
            with patch.object(appcast, "signature_details", return_value="TeamIdentifier=SAME\n"):
                appcast.verify_sparkle_layout(app, "TeamIdentifier=SAME\n")

    def test_missing_helper_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory)
            framework = self.layout(app)
            (framework / "Versions/B/Autoupdate").unlink()
            with patch.object(appcast, "signature_details", return_value="TeamIdentifier=SAME\n"):
                with self.assertRaises(appcast.ReleaseError):
                    appcast.verify_sparkle_layout(app, "TeamIdentifier=SAME\n")

    def test_nested_code_signed_by_other_team_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory)
            self.layout(app)
            with patch.object(appcast, "signature_details", return_value="TeamIdentifier=OTHER\n"):
                with self.assertRaises(appcast.ReleaseError):
                    appcast.verify_sparkle_layout(app, "TeamIdentifier=SAME\n")


if __name__ == "__main__":
    unittest.main()
