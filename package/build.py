#!/usr/bin/env python3
"""Sign and package Rebox into rootless .deb and TrollStore .ipa."""

from __future__ import annotations

import argparse
import hashlib
import io
import math
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_DIR = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import codesign  # noqa: E402

BUNDLE_ID = "com.bromhobit.Rebox"
APP_NAME = "Rebox"
RELEASE_NAME = "Rebox"
DEB_PACKAGE = "com.bromhobit.rebox"
LEGACY_DEB_PACKAGE = "org.codex.dopaminerenew"
DAEMON_LABEL = "com.bromhobit.rebox"
LEGACY_DAEMON_LABEL = "org.codex.dopamine-renew"
LIBEXEC_DIR = "/var/jb/usr/libexec/rebox"
DATA_DIR = "/var/mobile/Library/Rebox"
LEGACY_DATA_DIR = "/var/mobile/Library/DopamineRenew"
APP_BUNDLE_DIR = f"/var/jb/Applications/{APP_NAME}.app"
DAEMON_PLIST_DEST = f"/var/jb/Library/LaunchDaemons/{DAEMON_LABEL}.plist"
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"

FORBIDDEN_BUNDLE_NAMES = {
    "apple-session.plist",
    "anisette_state",
    "config.plist",
}


def _script_helpers() -> str:
    return (
        "find_bin() {\n"
        '    for p in "$@"; do\n'
        '        if [ -x "$p" ]; then\n'
        '            printf \'%s\\n\' "$p"\n'
        "            return 0\n"
        "        fi\n"
        "    done\n"
        "    return 1\n"
        "}\n"
        "\n"
        "sileo_finish() {\n"
        '    f="${1:-}"\n'
        '    [ -n "$f" ] && [ -n "${SILEO:-}" ] || return 0\n'
        "    set -- ${SILEO}\n"
        '    fd="${1:-}"\n'
        '    ver="${2:-0}"\n'
        '    case "$fd" in\n'
        "        ''|*[!0-9]*) return 0 ;;\n"
        "    esac\n"
        '    case "$ver" in\n'
        "        ''|*[!0-9]*) return 0 ;;\n"
        "    esac\n"
        '    [ "$fd" -gt 0 ] && [ "$ver" -ge 1 ] || return 0\n'
        '    printf \'finish:%s\\n\' "$f" >&"$fd" 2>/dev/null || true\n'
        "}\n"
    )


def build_postinst() -> bytes:
    return (
        "#!/var/jb/bin/sh\n"
        "\n"
        f'APP_PATH="{APP_BUNDLE_DIR}"\n'
        f'DAEMON_LABEL="{DAEMON_LABEL}"\n'
        f'LEGACY_DAEMON_LABEL="{LEGACY_DAEMON_LABEL}"\n'
        f'DAEMON_PLIST="{DAEMON_PLIST_DEST}"\n'
        f'LEGACY_DAEMON_PLIST="/var/jb/Library/LaunchDaemons/${{LEGACY_DAEMON_LABEL}}.plist"\n'
        f'DATA_DIR="{DATA_DIR}"\n'
        f'LEGACY_DATA_DIR="{LEGACY_DATA_DIR}"\n'
        "\n"
        + _script_helpers()
        + "\n"
        'case "${1:-configure}" in\n'
        "    configure)\n"
        '        if [ ! -L "$DATA_DIR" ]; then\n'
        '            mkdir -p "$DATA_DIR" >/dev/null 2>&1 || true\n'
        '            chmod 0700 "$DATA_DIR" >/dev/null 2>&1 || true\n'
        '            chown mobile:mobile "$DATA_DIR" >/dev/null 2>&1 || true\n'
        "        fi\n"
        "\n"
        '        if [ -d "$DATA_DIR" ] && [ ! -L "$DATA_DIR" ] && [ -d "$LEGACY_DATA_DIR" ] && [ ! -L "$LEGACY_DATA_DIR" ]; then\n'
        "            for fname in config.plist apple-session.plist anisette_state .sidebox-anisette-url .sidebox-session-link; do\n"
        '                src="${LEGACY_DATA_DIR}/${fname}"\n'
        '                dst="${DATA_DIR}/${fname}"\n'
        '                if [ -f "$src" ] && [ ! -L "$src" ] && [ ! -e "$dst" ] && [ ! -L "$dst" ]; then\n'
        '                    tmp="${DATA_DIR}/.${fname}.migrate.$$"\n'
        '                    if cp "$src" "$tmp" >/dev/null 2>&1; then\n'
        '                        chmod 0600 "$tmp" >/dev/null 2>&1 || true\n'
        '                        chown mobile:mobile "$tmp" >/dev/null 2>&1 || true\n'
        '                        mv -f "$tmp" "$dst" >/dev/null 2>&1 || rm -f "$tmp" >/dev/null 2>&1 || true\n'
        "                    else\n"
        '                        rm -f "$tmp" >/dev/null 2>&1 || true\n'
        "                    fi\n"
        "                fi\n"
        "            done\n"
        "        fi\n"
        "\n"
        '        if [ -f "$DAEMON_PLIST" ] && [ ! -L "$DAEMON_PLIST" ]; then\n'
        '            chown 0:0 "$DAEMON_PLIST" >/dev/null 2>&1 || true\n'
        '            chmod 0644 "$DAEMON_PLIST" >/dev/null 2>&1 || true\n'
        "        fi\n"
        "\n"
        '        if [ -n "${SILEO:-}" ]; then\n'
        "            sileo_finish uicache\n"
        "        else\n"
        '            UICACHE="$(find_bin /var/jb/usr/bin/uicache /usr/bin/uicache || true)"\n'
        '            if [ -n "$UICACHE" ] && [ -d "$APP_PATH" ]; then\n'
        '                "$UICACHE" -p "$APP_PATH" >/dev/null 2>&1 || true\n'
        "            fi\n"
        "        fi\n"
        "\n"
        '        LAUNCHCTL="$(find_bin /var/jb/bin/launchctl /var/jb/usr/bin/launchctl /bin/launchctl || true)"\n'
        '        if [ -n "$LAUNCHCTL" ]; then\n'
        '            "$LAUNCHCTL" bootout "system/${LEGACY_DAEMON_LABEL}" >/dev/null 2>&1 \\\n'
        '                || "$LAUNCHCTL" unload "$LEGACY_DAEMON_PLIST" >/dev/null 2>&1 \\\n'
        "                || true\n"
        '            if [ -f "$DAEMON_PLIST" ]; then\n'
        '                "$LAUNCHCTL" bootout "system/${DAEMON_LABEL}" >/dev/null 2>&1 \\\n'
        '                    || "$LAUNCHCTL" unload "$DAEMON_PLIST" >/dev/null 2>&1 \\\n'
        "                    || true\n"
        '                "$LAUNCHCTL" bootstrap system "$DAEMON_PLIST" >/dev/null 2>&1 \\\n'
        '                    || "$LAUNCHCTL" load -w "$DAEMON_PLIST" >/dev/null 2>&1 \\\n'
        "                    || true\n"
        "            fi\n"
        "        fi\n"
        "        ;;\n"
        "esac\n"
        "\n"
        "exit 0\n"
    ).encode("utf-8")


def build_prerm() -> bytes:
    return (
        "#!/var/jb/bin/sh\n"
        "\n"
        f'APP_PATH="{APP_BUNDLE_DIR}"\n'
        f'DAEMON_LABEL="{DAEMON_LABEL}"\n'
        f'DAEMON_PLIST="{DAEMON_PLIST_DEST}"\n'
        "\n"
        + _script_helpers()
        + "\n"
        'case "${1:-}" in\n'
        "    remove|deconfigure|upgrade|purge)\n"
        '        LAUNCHCTL="$(find_bin /var/jb/bin/launchctl /var/jb/usr/bin/launchctl /bin/launchctl || true)"\n'
        '        if [ -n "$LAUNCHCTL" ]; then\n'
        '            "$LAUNCHCTL" bootout "system/${DAEMON_LABEL}" >/dev/null 2>&1 \\\n'
        '                || "$LAUNCHCTL" unload "$DAEMON_PLIST" >/dev/null 2>&1 \\\n'
        "                || true\n"
        "        fi\n"
        "        ;;\n"
        "esac\n"
        "\n"
        'case "${1:-}" in\n'
        "    remove|deconfigure|purge)\n"
        '        UICACHE="$(find_bin /var/jb/usr/bin/uicache /usr/bin/uicache || true)"\n'
        '        if [ -n "$UICACHE" ] && [ -d "$APP_PATH" ]; then\n'
        '            "$UICACHE" -u "$APP_PATH" >/dev/null 2>&1 || true\n'
        "        fi\n"
        '        if [ -n "${SILEO:-}" ]; then\n'
        "            sileo_finish uicache\n"
        "        fi\n"
        "        ;;\n"
        "esac\n"
        "\n"
        "exit 0\n"
    ).encode("utf-8")


def build_postrm() -> bytes:
    return (
        "#!/var/jb/bin/sh\n"
        "\n"
        f'APP_PATH="{APP_BUNDLE_DIR}"\n'
        f'DAEMON_LABEL="{DAEMON_LABEL}"\n'
        f'DAEMON_PLIST="{DAEMON_PLIST_DEST}"\n'
        "\n"
        + _script_helpers()
        + "\n"
        'case "${1:-}" in\n'
        "    remove|purge|disappear)\n"
        '        LAUNCHCTL="$(find_bin /var/jb/bin/launchctl /var/jb/usr/bin/launchctl /bin/launchctl || true)"\n'
        '        if [ -n "$LAUNCHCTL" ]; then\n'
        '            "$LAUNCHCTL" bootout "system/${DAEMON_LABEL}" >/dev/null 2>&1 \\\n'
        '                || "$LAUNCHCTL" unload "$DAEMON_PLIST" >/dev/null 2>&1 \\\n'
        "                || true\n"
        "        fi\n"
        '        if [ -n "${SILEO:-}" ]; then\n'
        "            sileo_finish uicache\n"
        "        else\n"
        '            UICACHE="$(find_bin /var/jb/usr/bin/uicache /usr/bin/uicache || true)"\n'
        '            if [ -n "$UICACHE" ] && [ ! -d "$APP_PATH" ]; then\n'
        '                "$UICACHE" -p "$APP_PATH" >/dev/null 2>&1 || true\n'
        "            fi\n"
        "        fi\n"
        "        ;;\n"
        "esac\n"
        "\n"
        "exit 0\n"
    ).encode("utf-8")


def build_md5sums(data_entries: list[tuple[str, bytes | None, int]]) -> bytes:
    lines: list[str] = []
    for name, payload, _ in data_entries:
        if payload is None:
            continue
        rel = name[2:] if name.startswith("./") else name.lstrip("/")
        digest = hashlib.md5(payload).hexdigest()
        lines.append((rel, f"{digest}  {rel}"))
    lines.sort(key=lambda item: item[0])
    return ("\n".join(line for _, line in lines) + "\n").encode("utf-8")


def validate_shell_script(name: str, content: bytes) -> None:
    proc = subprocess.run(["sh", "-n"], input=content, capture_output=True)
    if proc.returncode != 0:
        err = proc.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(f"shell syntax check failed for {name}: {err}")


def make_tar_gz(
    entries: list[tuple[str, bytes | None, int]],
) -> bytes:
    """Create a deterministic GNU tar.gz archive (None payload -> directory entry)."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.GNU_FORMAT) as archive:
        for name, data, mode in entries:
            base = Path(name).name
            if base in FORBIDDEN_BUNDLE_NAMES or base.endswith(".mobileprovision"):
                raise RuntimeError(f"refusing to bundle sensitive file: {name}")
            info = tarfile.TarInfo(name)
            info.mode = mode
            info.uid = 0
            info.gid = 0
            info.uname = "root"
            info.gname = "wheel"
            info.mtime = 0
            if data is None:
                info.type = tarfile.DIRTYPE
                info.size = 0
                archive.addfile(info)
            else:
                info.type = tarfile.REGTYPE
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def build_debian_ar(members: list[tuple[str, bytes]]) -> bytes:
    """Write a standard Debian/BSD `ar` archive (no GNU trailing '/' on names)."""
    out = bytearray(b"!<arch>\n")
    for name, payload in members:
        if len(name) > 16 or "/" in name:
            raise ValueError(f"invalid Debian ar member name: {name!r}")
        header = (
            f"{name:<16}"
            f"{'0':<12}"
            f"{'0':<6}"
            f"{'0':<6}"
            f"{'100644':<8}"
            f"{len(payload):<10}"
            "`\n"
        ).encode("ascii")
        assert len(header) == 60
        out.extend(header)
        out.extend(payload)
        if len(payload) % 2 == 1:
            out.extend(b"\n")
    return bytes(out)


def verify_objdump_macho(binary: Path, *, require_signed: bool) -> None:
    proc = subprocess.run(
        ["llvm-objdump", "--macho", "--private-headers", str(binary)],
        text=True,
        capture_output=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"llvm-objdump failed on {binary}: {proc.stderr.strip()}")
    out = proc.stdout
    if "MH_MAGIC_64" not in out or "ARM64" not in out or "EXECUTE" not in out:
        raise RuntimeError(f"{binary} is not a 64-bit ARM64 Mach-O executable")
    if "platform ios" not in out:
        raise RuntimeError(f"{binary} does not target iOS (missing 'platform ios')")
    if require_signed and "LC_CODE_SIGNATURE" not in out:
        raise RuntimeError(f"{binary} is missing LC_CODE_SIGNATURE after signing")


def collect_app_icons(info_dict: dict, icons_dir: Path) -> list[tuple[str, bytes]]:
    """Collect PNG icons declared in Info.plist plus any AppIcon*.png in icons_dir."""
    names: list[str] = []

    def add_name(raw: object) -> None:
        if not isinstance(raw, str) or not raw:
            return
        if "/" in raw or "\\" in raw or raw in (".", ".."):
            raise RuntimeError(f"unsafe icon filename in Info.plist: {raw!r}")
        fname = raw if raw.endswith(".png") else f"{raw}.png"
        if fname not in names:
            names.append(fname)

    for item in info_dict.get("CFBundleIconFiles", []) or []:
        add_name(item)

    for icons_key in ("CFBundleIcons", "CFBundleIcons~ipad"):
        icons_section = info_dict.get(icons_key)
        if isinstance(icons_section, dict):
            primary = icons_section.get("CFBundlePrimaryIcon")
            if isinstance(primary, dict):
                for item in primary.get("CFBundleIconFiles", []) or []:
                    add_name(item)

    # Also discover standard AppIcon*.png files in icons_dir (supports @2x/@3x variants).
    if icons_dir.is_dir():
        for png in sorted(icons_dir.glob("AppIcon*.png")):
            if png.is_file() and png.name not in names:
                names.append(png.name)

    resolved: list[tuple[str, bytes]] = []
    for fname in names:
        candidate = icons_dir / fname
        if candidate.is_file():
            data = candidate.read_bytes()
            if not data.startswith(PNG_SIGNATURE):
                raise RuntimeError(f"icon {candidate} is not a valid PNG")
            resolved.append((fname, data))
        else:
            # Check @2x/@3x variants for base names like "AppIcon60x60" in CFBundleIcons
            stem = fname[:-4]
            matched_variant = False
            for suffix in ("@2x.png", "@3x.png"):
                vname = f"{stem}{suffix}"
                vpath = icons_dir / vname
                if vpath.is_file():
                    matched_variant = True
                    if all(existing != vname for existing, _ in resolved):
                        vdata = vpath.read_bytes()
                        if not vdata.startswith(PNG_SIGNATURE):
                            raise RuntimeError(f"icon {vpath} is not a valid PNG")
                        resolved.append((vname, vdata))
            if not matched_variant:
                raise RuntimeError(f"Info.plist references missing icon {fname} in {icons_dir}")

    if not resolved:
        raise RuntimeError(f"no app icon PNG files found in {icons_dir}")

    # Preserve deterministic sorted order
    resolved.sort(key=lambda pair: pair[0])
    return resolved


def validate_launchdaemon_plist(path: Path) -> bytes:
    raw = path.read_bytes()
    data = plistlib.loads(raw)
    if not isinstance(data, dict):
        raise RuntimeError(f"{path} is not a plist dictionary")
    if data.get("Label") != DAEMON_LABEL:
        raise RuntimeError(f"{path}: Label must be {DAEMON_LABEL!r}, got {data.get('Label')!r}")
    prog_args = data.get("ProgramArguments")
    expected_bin = f"{LIBEXEC_DIR}/rebox-controller"
    if not isinstance(prog_args, list) or not prog_args or prog_args[0] != expected_bin:
        raise RuntimeError(
            f"{path}: ProgramArguments[0] must be {expected_bin!r}, got {prog_args!r}"
        )
    if data.get("UserName") != "mobile":
        raise RuntimeError(f"{path}: UserName must be 'mobile', got {data.get('UserName')!r}")
    if data.get("Disabled") is not False:
        raise RuntimeError(f"{path}: Disabled must be false, got {data.get('Disabled')!r}")
    return plistlib.dumps(data, fmt=plistlib.FMT_XML, sort_keys=True)


def write_atomic(dest: Path, data: bytes) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{dest.name}.", dir=dest.parent)
    tmp_path = Path(tmp_name)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(tmp_path, 0o644)
        os.replace(tmp_path, dest)
    except Exception:
        tmp_path.unlink(missing_ok=True)
        raise


def build_ipa_bytes(
    *,
    app_binary: bytes,
    info_plist: bytes,
    icons: list[tuple[str, bytes]],
    helpers: list[tuple[str, bytes]],
) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_DEFLATED) as zf:
        for dirname in ("Payload/", f"Payload/{APP_NAME}.app/"):
            dinfo = zipfile.ZipInfo(dirname)
            dinfo.date_time = (2026, 1, 1, 0, 0, 0)
            dinfo.create_system = 3
            dinfo.external_attr = (0o040755 << 16) | 0x10
            zf.writestr(dinfo, b"")

        entries: list[tuple[str, bytes, int]] = [
            (f"Payload/{APP_NAME}.app/{APP_NAME}", app_binary, 0o755),
            (f"Payload/{APP_NAME}.app/Info.plist", info_plist, 0o644),
        ]
        for icon_name, icon_bytes in icons:
            entries.append((f"Payload/{APP_NAME}.app/{icon_name}", icon_bytes, 0o644))
        for helper_name, helper_bytes in helpers:
            entries.append((f"Payload/{APP_NAME}.app/{helper_name}", helper_bytes, 0o755))

        for name, data, mode in entries:
            base = Path(name).name
            if base in FORBIDDEN_BUNDLE_NAMES or base.endswith(".mobileprovision"):
                raise RuntimeError(f"refusing to bundle sensitive file into IPA: {name}")
            zinfo = zipfile.ZipInfo(name)
            zinfo.date_time = (2026, 1, 1, 0, 0, 0)
            zinfo.create_system = 3
            zinfo.external_attr = (0o100000 | mode) << 16
            zinfo.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(zinfo, data)
    return buf.getvalue()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Sign ARM64 iOS binaries and build Rebox rootless .deb and TrollStore .ipa."
    )
    parser.add_argument("--app-binary", type=Path, required=True, help="ARM64 iOS Rebox UI executable")
    parser.add_argument(
        "--controller-binary", type=Path, required=True, help="ARM64 iOS rebox-controller executable"
    )
    parser.add_argument("--profilectl-binary", type=Path, required=True, help="ARM64 iOS profilectl executable")
    parser.add_argument(
        "--apple-binary", type=Path, required=True, help="ARM64 iOS rebox-apple executable"
    )
    parser.add_argument(
        "--launchdaemon-plist",
        type=Path,
        default=REPO_DIR / "rootless" / f"{DAEMON_LABEL}.plist",
        help="LaunchDaemon plist path",
    )
    parser.add_argument(
        "--info-plist",
        type=Path,
        default=REPO_DIR / "ios-app" / "Info.plist",
        help="App Info.plist path",
    )
    parser.add_argument(
        "--icons-dir",
        type=Path,
        default=REPO_DIR / "ios-app",
        help="Directory containing AppIcon*.png files",
    )
    parser.add_argument(
        "--app-entitlements",
        type=Path,
        default=SCRIPT_DIR / "entitlements-app.plist",
        help="Entitlements XML plist for Rebox UI app",
    )
    parser.add_argument(
        "--helper-entitlements",
        type=Path,
        default=SCRIPT_DIR / "entitlements-helper.plist",
        help="Entitlements XML plist for helper binaries",
    )
    parser.add_argument("--output", type=Path, required=True, help="Output directory for .deb and .ipa")
    parser.add_argument("--version", default="1.0.1", help="Semantic version major.minor.patch (default: 1.0.1)")
    parser.add_argument(
        "--signer",
        choices=("auto", "ldid", "builtin"),
        default="auto",
        help="Code signing backend (default: auto)",
    )
    parser.add_argument("--ldid", type=Path, help="Optional explicit path to ldid binary")
    parser.add_argument("--force", action="store_true", help="Overwrite existing output .deb/.ipa files")
    args = parser.parse_args()

    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", args.version):
        parser.error("version must be a numeric major.minor.patch")

    args.output.mkdir(parents=True, exist_ok=True)
    deb_path = args.output / f"{RELEASE_NAME}-{args.version}-rootless.deb"
    ipa_path = args.output / f"{RELEASE_NAME}-{args.version}.ipa"
    if not args.force:
        for existing in (deb_path, ipa_path):
            if existing.exists():
                raise FileExistsError(f"refusing to overwrite {existing} (pass --force to overwrite)")

    # Validate inputs strictly upfront.
    app_ent_dict, _ = codesign.load_and_validate_entitlements(args.app_entitlements.resolve(strict=True))
    helper_ent_dict, _ = codesign.load_and_validate_entitlements(args.helper_entitlements.resolve(strict=True))

    source_info = plistlib.loads(args.info_plist.resolve(strict=True).read_bytes())
    if source_info.get("CFBundleIdentifier") != BUNDLE_ID or source_info.get("CFBundleExecutable") != APP_NAME:
        parser.error("unexpected CFBundleIdentifier or CFBundleExecutable in Info.plist")
    source_info["CFBundleShortVersionString"] = args.version
    source_info["CFBundleVersion"] = args.version
    info_plist_bytes = plistlib.dumps(source_info, fmt=plistlib.FMT_XML, sort_keys=True)

    icons = collect_app_icons(source_info, args.icons_dir.resolve(strict=True))
    daemon_plist_bytes = validate_launchdaemon_plist(args.launchdaemon_plist.resolve(strict=True))

    postinst_bytes = build_postinst()
    prerm_bytes = build_prerm()
    postrm_bytes = build_postrm()
    validate_shell_script("postinst", postinst_bytes)
    validate_shell_script("prerm", prerm_bytes)
    validate_shell_script("postrm", postrm_bytes)

    binary_specs = [
        (
            APP_NAME,
            args.app_binary.resolve(strict=True),
            args.app_entitlements.resolve(strict=True),
            app_ent_dict,
            BUNDLE_ID,
        ),
        (
            "rebox-controller",
            args.controller_binary.resolve(strict=True),
            args.helper_entitlements.resolve(strict=True),
            helper_ent_dict,
            f"{DAEMON_LABEL}.controller",
        ),
        (
            "profilectl",
            args.profilectl_binary.resolve(strict=True),
            args.helper_entitlements.resolve(strict=True),
            helper_ent_dict,
            f"{DAEMON_LABEL}.profilectl",
        ),
        (
            "rebox-apple",
            args.apple_binary.resolve(strict=True),
            args.helper_entitlements.resolve(strict=True),
            helper_ent_dict,
            f"{DAEMON_LABEL}.apple",
        ),
    ]

    signed_binaries: dict[str, bytes] = {}
    with tempfile.TemporaryDirectory(prefix="rebox-stage-") as td:
        stage_dir = Path(td)
        for name, src_path, ent_path, ent_dict, ident in binary_specs:
            if not src_path.is_file() or src_path.stat().st_size == 0:
                raise RuntimeError(f"invalid binary input: {src_path}")
            verify_objdump_macho(src_path, require_signed=False)
            staged = stage_dir / name
            backend = codesign.sign_macho_file(
                src_path,
                staged,
                identifier=ident,
                entitlements_path=ent_path,
                signer=args.signer,
                ldid_path=args.ldid,
            )
            verify_objdump_macho(staged, require_signed=True)
            signed_data = staged.read_bytes()
            sig_info = codesign.verify_macho_signature(
                signed_data,
                expected_entitlements=ent_dict,
                require_der=True,
            )
            signed_binaries[name] = signed_data
            print(
                f"Signed {name} ({backend}): ident={sig_info.identifier} "
                f"ver=0x{sig_info.version:x} flags=0x{sig_info.flags:x} "
                f"pages={sig_info.code_slots} cdhash={sig_info.cdhash_sha256}"
            )

    helpers: list[tuple[str, bytes]] = [
        ("rebox-controller", signed_binaries["rebox-controller"]),
        ("profilectl", signed_binaries["profilectl"]),
        ("rebox-apple", signed_binaries["rebox-apple"]),
    ]

    # Build data.tar.gz with explicit 0755 root:wheel parent directory entries.
    data_entries: list[tuple[str, bytes | None, int]] = [
        ("./", None, 0o755),
        ("./var", None, 0o755),
        ("./var/jb", None, 0o755),
        ("./var/jb/Applications", None, 0o755),
        (f".{APP_BUNDLE_DIR}", None, 0o755),
        (f".{APP_BUNDLE_DIR}/{APP_NAME}", signed_binaries[APP_NAME], 0o755),
        (f".{APP_BUNDLE_DIR}/Info.plist", info_plist_bytes, 0o644),
    ]
    for icon_name, icon_bytes in icons:
        data_entries.append((f".{APP_BUNDLE_DIR}/{icon_name}", icon_bytes, 0o644))
    for helper_name, helper_bytes in helpers:
        data_entries.append((f".{APP_BUNDLE_DIR}/{helper_name}", helper_bytes, 0o755))

    data_entries.extend(
        [
            ("./var/jb/usr", None, 0o755),
            ("./var/jb/usr/libexec", None, 0o755),
            (f".{LIBEXEC_DIR}", None, 0o755),
        ]
    )
    for helper_name, helper_bytes in helpers:
        data_entries.append((f".{LIBEXEC_DIR}/{helper_name}", helper_bytes, 0o755))

    data_entries.extend(
        [
            ("./var/jb/Library", None, 0o755),
            ("./var/jb/Library/LaunchDaemons", None, 0o755),
            (f".{DAEMON_PLIST_DEST}", daemon_plist_bytes, 0o644),
        ]
    )

    installed_bytes = sum(len(payload) for _, payload, _ in data_entries if payload is not None)
    installed_kib = max(1, math.ceil(installed_bytes / 1024))
    md5sums_bytes = build_md5sums(data_entries)
    control_bytes = (
        f"Package: {DEB_PACKAGE}\n"
        f"Name: {RELEASE_NAME}\n"
        f"Version: {args.version}\n"
        "Architecture: iphoneos-arm64\n"
        "Maintainer: bromhobit <304797858+bromhobit@users.noreply.github.com>\n"
        "Author: bromhobit <304797858+bromhobit@users.noreply.github.com>\n"
        "Section: Utilities\n"
        "Priority: optional\n"
        f"Conflicts: {LEGACY_DEB_PACKAGE}\n"
        f"Replaces: {LEGACY_DEB_PACKAGE}\n"
        "Homepage: https://github.com/bromhobit/Rebox\n"
        f"Installed-Size: {installed_kib}\n"
        "Description: iOS provisioning profile monitor and auto-renewal tool\n"
        " Rebox monitors installed iOS sideloading provisioning profiles and\n"
        " automatically renews them via Apple Developer with a rootless LaunchDaemon.\n"
    ).encode("utf-8")

    control_tar = make_tar_gz(
        [
            ("./", None, 0o755),
            ("./control", control_bytes, 0o644),
            ("./md5sums", md5sums_bytes, 0o644),
            ("./postinst", postinst_bytes, 0o755),
            ("./prerm", prerm_bytes, 0o755),
            ("./postrm", postrm_bytes, 0o755),
        ]
    )
    data_tar = make_tar_gz(data_entries)
    deb_bytes = build_debian_ar(
        [
            ("debian-binary", b"2.0\n"),
            ("control.tar.gz", control_tar),
            ("data.tar.gz", data_tar),
        ]
    )

    ipa_bytes = build_ipa_bytes(
        app_binary=signed_binaries[APP_NAME],
        info_plist=info_plist_bytes,
        icons=icons,
        helpers=helpers,
    )

    write_atomic(deb_path, deb_bytes)
    write_atomic(ipa_path, ipa_bytes)

    print(f"Icons bundled: {', '.join(name for name, _ in icons)}")
    print(f"Rootless package: {deb_path} ({len(deb_bytes)} bytes)")
    print(f"IPA: {ipa_path} ({len(ipa_bytes)} bytes)")


if __name__ == "__main__":
    main()
