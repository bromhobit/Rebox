"""Regression checks for Rebox branding, identifiers, and rootless package integrity."""
import hashlib
import io
import plistlib
import subprocess
import sys
import tarfile
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "package"))

import build as pkg_build  # noqa: E402
import codesign  # noqa: E402


def _read_deb_members(deb_path: Path) -> dict[str, bytes]:
    raw = deb_path.read_bytes()
    assert raw.startswith(b"!<arch>\n"), f"invalid ar header in {deb_path}"
    pos = 8
    members: dict[str, bytes] = {}
    while pos < len(raw):
        hdr = raw[pos : pos + 60]
        name = hdr[:16].decode("ascii").strip()
        size = int(hdr[48:58].decode("ascii").strip())
        members[name] = raw[pos + 60 : pos + 60 + size]
        pos += 60 + size + (size % 2)
    return members


class ReboxBrandingTests(unittest.TestCase):
    def test_ios_ui_uses_rebox_names(self):
        src = (ROOT / "ios-app/src/ui.rs").read_text(encoding="utf-8")
        self.assertIn('"REBOX"', src)
        self.assertIn("ReboxDelegate", src)
        self.assertIn("ReboxController", src)
        self.assertNotIn('"DOPAMINE RENEW"', src)
        self.assertNotIn("RenewDelegate", src)
        self.assertNotIn("RenewController", src)

    def test_info_plist_uses_rebox_identity(self):
        info = plistlib.loads((ROOT / "ios-app/Info.plist").read_bytes())
        self.assertEqual(info["CFBundleDisplayName"], "Rebox")
        self.assertEqual(info["CFBundleName"], "Rebox")
        self.assertEqual(info["CFBundleExecutable"], "Rebox")
        self.assertEqual(info["CFBundleIdentifier"], "com.bromhobit.Rebox")
        self.assertEqual(info["CFBundleShortVersionString"], "1.0.1")
        self.assertEqual(info["CFBundleVersion"], "1.0.1")

    def test_launchdaemon_plist_uses_rebox_identity(self):
        old_plist = ROOT / "rootless/org.codex.dopamine-renew.plist"
        new_plist = ROOT / "rootless/com.bromhobit.rebox.plist"
        self.assertFalse(old_plist.exists())
        self.assertTrue(new_plist.is_file())
        data = plistlib.loads(new_plist.read_bytes())
        self.assertEqual(data["Label"], "com.bromhobit.rebox")
        self.assertEqual(
            data["ProgramArguments"],
            ["/var/jb/usr/libexec/rebox/rebox-controller", "auto"],
        )
        self.assertEqual(data["StandardOutPath"], "/var/mobile/Library/Rebox/daemon.log")
        self.assertEqual(data["StandardErrorPath"], "/var/mobile/Library/Rebox/daemon.log")

    def test_maintainer_scripts_rootless_safe(self):
        scripts = {
            "postinst": pkg_build.build_postinst(),
            "prerm": pkg_build.build_prerm(),
            "postrm": pkg_build.build_postrm(),
        }
        for name, raw in scripts.items():
            text = raw.decode("utf-8")
            self.assertTrue(text.startswith("#!/var/jb/bin/sh\n"), f"{name} shebang")
            self.assertNotIn("\r", text, f"{name} must not have CRLF")
            self.assertNotIn("set -e", text, f"{name} must not trap dpkg with set -e")
            self.assertTrue(text.rstrip().endswith("exit 0"), f"{name} must exit 0")
            proc = subprocess.run(["sh", "-n"], input=raw, capture_output=True)
            self.assertEqual(proc.returncode, 0, proc.stderr.decode("utf-8", errors="replace"))
        self.assertIn("remove|deconfigure|purge", scripts["prerm"].decode("utf-8"))
        self.assertIn("remove|purge|disappear", scripts["postrm"].decode("utf-8"))

    def test_release_archives_when_present(self):
        deb_path = ROOT / "dist/Rebox-1.0.1-rootless.deb"
        ipa_path = ROOT / "dist/Rebox-1.0.1.ipa"
        if not (deb_path.is_file() and ipa_path.is_file()):
            self.skipTest("dist/Rebox-1.0.1 artifacts not built yet")

        members = _read_deb_members(deb_path)
        self.assertEqual(
            set(members.keys()),
            {"debian-binary", "control.tar.gz", "data.tar.gz"},
        )
        self.assertEqual(members["debian-binary"], b"2.0\n")

        control_files: dict[str, tuple[int, bytes]] = {}
        with tarfile.open(fileobj=io.BytesIO(members["control.tar.gz"]), mode="r:gz") as tf:
            for ti in tf.getmembers():
                if ti.isfile():
                    control_files[ti.name] = (ti.mode & 0o777, tf.extractfile(ti).read())

        self.assertIn("./control", control_files)
        self.assertIn("./md5sums", control_files)
        for script in ("./postinst", "./prerm", "./postrm"):
            self.assertIn(script, control_files)
            mode, content = control_files[script]
            self.assertEqual(mode, 0o755, script)
            self.assertTrue(content.startswith(b"#!/var/jb/bin/sh\n"), script)

        control_text = control_files["./control"][1].decode("utf-8")
        self.assertIn("Package: com.bromhobit.rebox\n", control_text)
        self.assertIn("Name: Rebox\n", control_text)
        self.assertIn("Version: 1.0.1\n", control_text)
        self.assertIn("Architecture: iphoneos-arm64\n", control_text)
        self.assertIn("Conflicts: org.codex.dopaminerenew\n", control_text)
        self.assertIn("Replaces: org.codex.dopaminerenew\n", control_text)
        self.assertNotIn("Provides:", control_text)

        data_hashes: dict[str, str] = {}
        app_ent = codesign.load_and_validate_entitlements(
            ROOT / "package/entitlements-app.plist"
        )[0]
        helper_ent = codesign.load_and_validate_entitlements(
            ROOT / "package/entitlements-helper.plist"
        )[0]
        checked_binaries = 0

        with tarfile.open(fileobj=io.BytesIO(members["data.tar.gz"]), mode="r:gz") as tf:
            for ti in tf.getmembers():
                self.assertNotIn("DopamineRenew", ti.name)
                self.assertNotIn("dopamine-renew", ti.name)
                if ti.isfile():
                    payload = tf.extractfile(ti).read()
                    rel = ti.name[2:] if ti.name.startswith("./") else ti.name.lstrip("/")
                    data_hashes[rel] = hashlib.md5(payload).hexdigest()
                    if ti.name == "./var/jb/Applications/Rebox.app/Rebox":
                        sig = codesign.verify_macho_signature(
                            payload, expected_entitlements=app_ent, require_der=True
                        )
                        self.assertEqual(sig.identifier, "com.bromhobit.Rebox")
                        checked_binaries += 1
                    elif ti.name in (
                        "./var/jb/usr/libexec/rebox/rebox-controller",
                        "./var/jb/usr/libexec/rebox/rebox-apple",
                        "./var/jb/usr/libexec/rebox/profilectl",
                    ):
                        sig = codesign.verify_macho_signature(
                            payload, expected_entitlements=helper_ent, require_der=True
                        )
                        self.assertTrue(sig.identifier.startswith("com.bromhobit.rebox."))
                        checked_binaries += 1

        self.assertEqual(checked_binaries, 4)

        md5_entries: dict[str, str] = {}
        for line in control_files["./md5sums"][1].decode("utf-8").splitlines():
            digest, path = line.split("  ", 1)
            md5_entries[path] = digest
        self.assertEqual(md5_entries, data_hashes)

        with zipfile.ZipFile(ipa_path, "r") as zf:
            names = zf.namelist()
            for name in names:
                self.assertNotIn("DopamineRenew", name)
                self.assertNotIn("dopamine-renew", name)
            self.assertIn("Payload/Rebox.app/Rebox", names)
            self.assertIn("Payload/Rebox.app/rebox-controller", names)
            self.assertIn("Payload/Rebox.app/rebox-apple", names)
            self.assertIn("Payload/Rebox.app/profilectl", names)
            info = plistlib.loads(zf.read("Payload/Rebox.app/Info.plist"))
            self.assertEqual(info["CFBundleIdentifier"], "com.bromhobit.Rebox")
            self.assertEqual(info["CFBundleExecutable"], "Rebox")
            self.assertEqual(info["CFBundleVersion"], "1.0.1")


if __name__ == "__main__":
    unittest.main()
