#!/usr/bin/env python3
"""ShellX release entitlements stay ShellX-only (no cmux team, no tunnel extension)."""

import plistlib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ENTITLEMENTS = ROOT / "shellx.release.entitlements"


class ShellXEntitlementsTests(unittest.TestCase):
    def test_release_entitlements_are_shellx_only(self):
        with open(ENTITLEMENTS, "rb") as f:
            ent = plistlib.load(f)
        groups = ent.get("com.apple.security.application-groups", [])
        self.assertEqual(groups, ["W4RF3W4VTZ.pro.shellx.desktop"])
        self.assertTrue(all("7WLXT3NR37" not in str(v) for v in ent.values()))
        self.assertNotIn("com.apple.developer.networking.networkextension", ent)
        self.assertNotIn("com.apple.developer.system-extension.install", ent)

    def test_no_profile_restricted_entitlements(self):
        # Without a Developer ID provisioning profile, amfi refuses to spawn an app
        # that claims these (launchd errno 163), so the bundle never opens.
        with open(ENTITLEMENTS, "rb") as f:
            ent = plistlib.load(f)
        for key in (
            "com.apple.application-identifier",
            "com.apple.developer.team-identifier",
            "keychain-access-groups",
            "com.apple.developer.web-browser.public-key-credential",
        ):
            self.assertNotIn(key, ent)


if __name__ == "__main__":
    unittest.main()
