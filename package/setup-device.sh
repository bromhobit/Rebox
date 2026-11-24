#!/usr/bin/env bash
# Deploy Rebox rootless .deb, generate config.plist from .mobileprovision,
# and copy an existing Apple session to a jailbroken iPhone with 0700/0600 permissions.
# Never hardcodes credentials or prints session tokens/secrets.

set -euo pipefail
umask 077

HOST="127.0.0.1"
PORT="2222"
USER_NAME="root"
DEB_PATH=""
SESSION_DIR=""
DEVICE_UDID=""
RENEW_BEFORE_HOURS="48"
CONFIG_ONLY_PATH=""
DRY_RUN=0
PROFILES=()
SSH_EXTRA_OPTS=()

usage() {
    cat <<'EOF'
Usage: setup-device.sh [OPTIONS]

Deploy Rebox to a Dopamine rootless iPhone over SSH (e.g. via iproxy 2222 22)
or generate config.plist locally from .mobileprovision profile(s).

Options:
  --deb PATH                 Rootless .deb package to install via dpkg -i
  --profile PATH             Path to .mobileprovision (repeatable for multiple apps)
  --session-dir DIR          Local dir containing apple-session.plist (+ optional anisette_state)
  --device UDID              Explicit iPhone UDID (optional if profile has 1 device)
  --renew-before-hours N     Renewal window in hours (default: 48)
  --config-only PATH         Write generated config.plist locally (0600) and exit (no SSH)
  --host HOST                SSH host (default: 127.0.0.1)
  --port PORT                SSH port (default: 2222)
  --user USER                SSH user (default: root)
  --identity PATH            Optional SSH private key (-i)
  --dry-run                  Validate inputs and print planned actions without modifying device
  -h, --help                 Show this help message

Examples:
  # 1. Start USB tunnel in another terminal: iproxy 2222 22
  # 2. Deploy .deb + config + existing Apple session:
  ./setup-device.sh \
    --port 2222 \
    --deb ../dist/Rebox-1.0.1-rootless.deb \
    --profile /path/to/sidestore.mobileprovision \
    --session-dir /path/to/rebox-apple-session

  # Generate config.plist locally without touching a device:
  ./setup-device.sh --profile /path/to/sidestore.mobileprovision --config-only ./config.plist
EOF
}

die() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

require_value() {
    if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        die "Option $1 requires a value"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --deb)
            require_value "$@"
            DEB_PATH="$2"
            shift 2
            ;;
        --profile)
            require_value "$@"
            PROFILES+=("$2")
            shift 2
            ;;
        --session-dir)
            require_value "$@"
            SESSION_DIR="$2"
            shift 2
            ;;
        --device)
            require_value "$@"
            DEVICE_UDID="$2"
            shift 2
            ;;
        --renew-before-hours)
            require_value "$@"
            RENEW_BEFORE_HOURS="$2"
            shift 2
            ;;
        --config-only)
            require_value "$@"
            CONFIG_ONLY_PATH="$2"
            shift 2
            ;;
        --host)
            require_value "$@"
            HOST="$2"
            shift 2
            ;;
        --port)
            require_value "$@"
            PORT="$2"
            shift 2
            ;;
        --user)
            require_value "$@"
            USER_NAME="$2"
            shift 2
            ;;
        --identity)
            require_value "$@"
            [ -f "$2" ] || die "SSH identity file not found: $2"
            SSH_EXTRA_OPTS+=(-i "$2")
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "Unknown option: $1 (use --help)"
            ;;
    esac
done

case "$PORT" in
    ''|*[!0-9]*) die "Invalid --port: $PORT" ;;
esac
if [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
    die "Port out of range: $PORT"
fi

case "$RENEW_BEFORE_HOURS" in
    ''|*[!0-9]*) die "Invalid --renew-before-hours: $RENEW_BEFORE_HOURS" ;;
esac
if [ "$RENEW_BEFORE_HOURS" -lt 1 ] || [ "$RENEW_BEFORE_HOURS" -gt 720 ]; then
    die "--renew-before-hours must be between 1 and 720"
fi

if [ -n "$CONFIG_ONLY_PATH" ] && [ "${#PROFILES[@]}" -eq 0 ]; then
    die "--config-only requires at least one --profile"
fi

if [ -z "$CONFIG_ONLY_PATH" ] && [ -z "$DEB_PATH" ] && [ "${#PROFILES[@]}" -eq 0 ] && [ -z "$SESSION_DIR" ]; then
    die "Nothing to do: pass at least one of --deb, --profile, --session-dir, or --config-only"
fi

if [ -n "$DEB_PATH" ]; then
    [ -f "$DEB_PATH" ] && [ ! -L "$DEB_PATH" ] || die ".deb file not found or is a symlink: $DEB_PATH"
fi

SESSION_PLIST=""
ANISETTE_STATE=""
if [ -n "$SESSION_DIR" ]; then
    [ -d "$SESSION_DIR" ] && [ ! -L "$SESSION_DIR" ] || die "Session directory invalid: $SESSION_DIR"
    SESSION_PLIST="$SESSION_DIR/apple-session.plist"
    [ -f "$SESSION_PLIST" ] && [ ! -L "$SESSION_PLIST" ] || die "Missing apple-session.plist in $SESSION_DIR"
    if [ -e "$SESSION_DIR/anisette_state" ]; then
        [ -f "$SESSION_DIR/anisette_state" ] && [ ! -L "$SESSION_DIR/anisette_state" ] || die "Invalid anisette_state in $SESSION_DIR"
        ANISETTE_STATE="$SESSION_DIR/anisette_state"
    fi
fi

for p in "${PROFILES[@]}"; do
    [ -f "$p" ] && [ ! -L "$p" ] || die "Profile file not found or invalid: $p"
done

WORK_TMP="$(mktemp -d "${TMPDIR:-/tmp}/rebox-setup-XXXXXX")"
chmod 0700 "$WORK_TMP"
CTL_SOCK="$WORK_TMP/ssh.sock"
REMOTE="${USER_NAME}@${HOST}"
CTL_OPEN=0

cleanup() {
    if [ "$CTL_OPEN" -eq 1 ]; then
        ssh -S "$CTL_SOCK" -O exit "$REMOTE" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK_TMP"
}
trap cleanup EXIT

GENERATED_CONFIG=""
if [ "${#PROFILES[@]}" -gt 0 ]; then
    command -v python3 >/dev/null 2>&1 || die "python3 is required to parse .mobileprovision profiles"
    GENERATED_CONFIG="$WORK_TMP/config.plist"
    python3 - "$GENERATED_CONFIG" "$DEVICE_UDID" "$RENEW_BEFORE_HOURS" "${PROFILES[@]}" <<'PY'
import os
import plistlib
import shutil
import subprocess
import sys
from pathlib import Path

out_path = Path(sys.argv[1])
explicit_udid = sys.argv[2].strip()
renew_before_hours = int(sys.argv[3])
profile_paths = [Path(p) for p in sys.argv[4:]]


def load_profile_plist(path: Path) -> dict:
    raw = path.read_bytes()
    openssl = shutil.which("openssl")
    if openssl:
        proc = subprocess.run(
            [openssl, "cms", "-verify", "-noverify", "-inform", "DER"],
            input=raw,
            capture_output=True,
        )
        if proc.returncode == 0 and proc.stdout:
            return plistlib.loads(proc.stdout)
    start = raw.find(b"<?xml")
    end = raw.find(b"</plist>")
    if start < 0 or end < 0:
        raise SystemExit(f"Could not decode plist payload from {path}")
    return plistlib.loads(raw[start : end + len(b"</plist>")])


team_id = None
selected_udid = explicit_udid or None
bundle_entries: list[dict[str, str]] = []
seen_bundles: set[str] = set()

for ppath in profile_paths:
    pl = load_profile_plist(ppath)
    teams = pl.get("TeamIdentifier") or []
    if not teams or not isinstance(teams[0], str):
        raise SystemExit(f"Profile {ppath} is missing TeamIdentifier")
    p_team = teams[0]
    if team_id is None:
        team_id = p_team
    elif team_id != p_team:
        raise SystemExit(f"TeamIdentifier mismatch across profiles: {team_id} vs {p_team}")

    devices = [d for d in (pl.get("ProvisionedDevices") or []) if isinstance(d, str)]
    if not devices:
        raise SystemExit(f"Profile {ppath} has no ProvisionedDevices")
    if selected_udid is None:
        if len(devices) == 1:
            selected_udid = devices[0]
        else:
            raise SystemExit(
                f"Profile {ppath} contains {len(devices)} devices; pass --device UDID explicitly"
            )
    elif not any(d.lower() == selected_udid.lower() for d in devices):
        raise SystemExit(f"Device {selected_udid} is not provisioned in {ppath}")

    ent = pl.get("Entitlements") or {}
    app_id = ent.get("application-identifier")
    if not isinstance(app_id, str) or "." not in app_id:
        raise SystemExit(f"Profile {ppath} is missing Entitlements.application-identifier")
    prefix, bundle_id = app_id.split(".", 1)
    if prefix != team_id or "*" in bundle_id or not bundle_id:
        raise SystemExit(f"Unsupported or wildcard application-identifier in {ppath}: {app_id}")
    if bundle_id not in seen_bundles:
        seen_bundles.add(bundle_id)
        entry: dict[str, str] = {"bundle": bundle_id}
        app_name = pl.get("AppIDName")
        if isinstance(app_name, str) and app_name.strip():
            entry["name"] = app_name.strip()
        bundle_entries.append(entry)

assert team_id is not None and selected_udid is not None and bundle_entries

config: dict[str, object] = {
    "team": team_id,
    "device": selected_udid,
    "bundle": bundle_entries[0]["bundle"],
    "bundles": bundle_entries,
    "renew_before_hours": renew_before_hours,
}
if "name" in bundle_entries[0]:
    config["name"] = bundle_entries[0]["name"]

payload = plistlib.dumps(config, fmt=plistlib.FMT_XML, sort_keys=True)
fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
with os.fdopen(fd, "wb") as handle:
    handle.write(payload)
    handle.flush()
    os.fsync(handle.fileno())

print(
    f"Prepared config.plist: team={team_id} device={selected_udid} "
    f"bundles={','.join(e['bundle'] for e in bundle_entries)} renew_before_hours={renew_before_hours}"
)
PY
fi

if [ -n "$CONFIG_ONLY_PATH" ]; then
    if [ "$DRY_RUN" -eq 1 ]; then
        printf '[dry-run] Would write generated config.plist to %s (mode 0600)\n' "$CONFIG_ONLY_PATH"
        exit 0
    fi
    if [ -L "$CONFIG_ONLY_PATH" ]; then
        die "Refusing to overwrite symlink: $CONFIG_ONLY_PATH"
    fi
    install -m 0600 "$GENERATED_CONFIG" "$CONFIG_ONLY_PATH"
    printf 'Wrote %s (mode 0600)\n' "$CONFIG_ONLY_PATH"
    exit 0
fi

if [ "$DRY_RUN" -eq 1 ]; then
    printf '[dry-run] Target SSH: %s (port %s)\n' "$REMOTE" "$PORT"
    [ -n "$DEB_PATH" ] && printf '[dry-run] Would install .deb: %s\n' "$DEB_PATH"
    [ -n "$GENERATED_CONFIG" ] && printf '[dry-run] Would upload config.plist -> /var/mobile/Library/Rebox/config.plist (0600 mobile:mobile)\n'
    [ -n "$SESSION_PLIST" ] && printf '[dry-run] Would upload apple-session.plist -> /var/mobile/Library/Rebox/apple-session.plist (0600 mobile:mobile)\n'
    [ -n "$ANISETTE_STATE" ] && printf '[dry-run] Would upload anisette_state -> /var/mobile/Library/Rebox/anisette_state (0600 mobile:mobile)\n'
    exit 0
fi

command -v ssh >/dev/null 2>&1 || die "ssh client is required"

SSH_BASE=(
    -p "$PORT"
    -o ConnectTimeout=10
    -o StrictHostKeyChecking=accept-new
    "${SSH_EXTRA_OPTS[@]}"
)

printf 'Connecting to %s:%s...\n' "$REMOTE" "$PORT"
ssh "${SSH_BASE[@]}" -M -S "$CTL_SOCK" -fN "$REMOTE"
CTL_OPEN=1

SSH_RUN=(
    ssh
    -S "$CTL_SOCK"
    -o ControlMaster=no
    "$REMOTE"
)

if [ -n "$DEB_PATH" ]; then
    printf 'Installing %s on device...\n' "$(basename "$DEB_PATH")"
    "${SSH_RUN[@]}" 'sh -eu -c '\''
        export PATH="/var/jb/usr/local/sbin:/var/jb/usr/local/bin:/var/jb/usr/sbin:/var/jb/usr/bin:/var/jb/sbin:/var/jb/bin:$PATH"
        umask 077
        tmpdir="$(mktemp -d /tmp/rebox.XXXXXX)"
        chmod 0700 "$tmpdir"
        trap "rm -rf \"$tmpdir\"" EXIT
        tmp="$tmpdir/package.deb"
        cat > "$tmp"
        if [ -x /var/jb/usr/bin/dpkg ]; then
            DPKG=/var/jb/usr/bin/dpkg
        else
            DPKG="$(command -v dpkg)"
        fi
        "$DPKG" -i "$tmp"
    '\''' < "$DEB_PATH"
fi

upload_private_file() {
    local local_file="$1"
    local remote_name="$2"
    case "$remote_name" in
        config.plist|apple-session.plist|anisette_state) ;;
        *) die "Internal error: disallowed remote filename $remote_name" ;;
    esac
    printf 'Uploading %s to /var/mobile/Library/Rebox/%s (0600)...\n' \
        "$remote_name" "$remote_name"
    "${SSH_RUN[@]}" 'sh -eu -c '\''
        export PATH="/var/jb/usr/local/sbin:/var/jb/usr/local/bin:/var/jb/usr/sbin:/var/jb/usr/bin:/var/jb/sbin:/var/jb/bin:$PATH"
        umask 077
        remote_name="$1"
        dir="/var/mobile/Library/Rebox"
        if [ -L "$dir" ]; then
            echo "Refusing symlink $dir" >&2
            exit 1
        fi
        mkdir -p "$dir"
        chmod 0700 "$dir"
        if [ "$(id -u)" -eq 0 ]; then
            chown mobile:mobile "$dir"
        fi
        dest="$dir/$remote_name"
        if [ -L "$dest" ]; then
            echo "Refusing symlink $dest" >&2
            exit 1
        fi
        tmp="$dir/.$remote_name.tmp.$$"
        trap "rm -f \"$tmp\"" EXIT
        cat > "$tmp"
        chmod 0600 "$tmp"
        if [ "$(id -u)" -eq 0 ]; then
            chown mobile:mobile "$tmp"
        fi
        mv -f "$tmp" "$dest"
        trap - EXIT
    '\'' _ '"$remote_name" < "$local_file"
}

if [ -n "$GENERATED_CONFIG" ]; then
    upload_private_file "$GENERATED_CONFIG" "config.plist"
fi

if [ -n "$SESSION_PLIST" ]; then
    upload_private_file "$SESSION_PLIST" "apple-session.plist"
fi

if [ -n "$ANISETTE_STATE" ]; then
    upload_private_file "$ANISETTE_STATE" "anisette_state"
fi

printf 'Verifying controller status on device...\n'
"${SSH_RUN[@]}" 'sh -eu -c '\''
    export PATH="/var/jb/usr/local/sbin:/var/jb/usr/local/bin:/var/jb/usr/sbin:/var/jb/usr/bin:/var/jb/sbin:/var/jb/bin:$PATH"
    CTRL="/var/jb/usr/libexec/rebox/rebox-controller"
    if [ ! -x "$CTRL" ]; then
        CTRL="/var/jb/Applications/Rebox.app/rebox-controller"
    fi
    if [ -x "$CTRL" ]; then
        if [ "$(id -u)" -eq 0 ] && command -v su >/dev/null 2>&1; then
            su -s /bin/sh mobile -c "$CTRL status" || true
        else
            "$CTRL" status || true
        fi
    else
        echo "Note: rebox-controller not installed yet (pass --deb to install)."
    fi
'\''' </dev/null

printf 'Device setup complete.\n'
