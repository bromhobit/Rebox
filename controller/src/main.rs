mod sidebox_bridge;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env,
    ffi::OsStr,
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const BASE: &str = "/var/mobile/Library/Rebox";
const LEGACY_BASE: &str = "/var/mobile/Library/DopamineRenew";
const LIBEXEC_DIR: &str = "/var/jb/usr/libexec/rebox";
const LEGACY_BIN_DIR: &str = "/var/mobile/Library/Rebox/bin";
const DAEMON_PLIST: &str = "/var/jb/Library/LaunchDaemons/com.bromhobit.rebox.plist";
const LEGACY_MIGRATION_ALLOWLIST: &[&str] = &[
    "config.plist",
    "apple-session.plist",
    "anisette_state",
    ".sidebox-anisette-url",
    ".sidebox-session-link",
];
const MAX_LEGACY_FILE_BYTES: u64 = 1024 * 1024;
const DEFAULT_RENEW_BEFORE_HOURS: u64 = 48;
const MIN_RENEW_BEFORE_HOURS: u64 = 1;
const MAX_RENEW_BEFORE_HOURS: u64 = 720;
const MAX_BUNDLE_ID_LEN: usize = 255;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum BundleEntry {
    Id(String),
    Detailed {
        bundle: String,
        #[serde(default)]
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct RawConfig {
    team: String,
    device: String,
    #[serde(default)]
    bundle: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    bundles: Option<Vec<BundleEntry>>,
    #[serde(default)]
    renew_before_hours: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedApp {
    bundle: String,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    team: String,
    device: String,
    apps: Vec<TrackedApp>,
    renew_before_hours: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct AppStatus {
    name: String,
    bundle_id: String,
    status: String,
    expires_at_unix: Option<u64>,
    hours_remaining: Option<u64>,
    needs_renewal: bool,
    renewed: bool,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ControllerReport {
    schema_version: u32,
    command: String,
    status: String,
    message: String,
    config_exists: bool,
    apple_session_exists: bool,
    apple_session_valid: bool,
    daemon_installed: bool,
    daemon_enabled: bool,
    renew_before_hours: u64,
    checked_at_unix: u64,
    apps: Vec<AppStatus>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemState {
    config_exists: bool,
    apple_session_exists: bool,
    apple_session_valid: bool,
    apple_session_error: Option<String>,
    daemon_installed: bool,
    daemon_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DiscoveredProfile {
    bundle_id: String,
    application_identifier: String,
    app_name: String,
    profile_name: String,
    team_id: String,
    #[serde(default)]
    team_name: String,
    uuid: String,
    expires_at_unix: u64,
    #[serde(default)]
    creation_date_unix: Option<u64>,
    #[serde(default)]
    is_wildcard: bool,
    #[serde(default)]
    provisioned_devices: Vec<String>,
}

#[derive(Debug, Clone)]
struct RuntimeEnv {
    base: PathBuf,
    daemon_plist: PathBuf,
    helper_dirs: Vec<PathBuf>,
}

impl RuntimeEnv {
    fn production() -> Self {
        let exe_dir = env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf));
        Self {
            base: PathBuf::from(BASE),
            daemon_plist: PathBuf::from(DAEMON_PLIST),
            helper_dirs: default_helper_dirs(exe_dir.as_deref()),
        }
    }
}

fn default_helper_dirs(exe_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::with_capacity(3);
    if let Some(dir) = exe_dir {
        dirs.push(dir.to_path_buf());
    }
    for fixed in [LIBEXEC_DIR, LEGACY_BIN_DIR] {
        let path = PathBuf::from(fixed);
        if !dirs.contains(&path) {
            dirs.push(path);
        }
    }
    dirs
}

fn resolve_helper_in(name: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in search_dirs {
        let candidate = dir.join(name);
        if let Ok(meta) = fs::metadata(&candidate) {
            if meta.is_file() && (meta.permissions().mode() & 0o111 != 0) {
                return Some(candidate);
            }
        }
    }
    None
}

fn validate_bundle_id(bundle: &str) -> Result<(), String> {
    if bundle.is_empty()
        || bundle.len() > MAX_BUNDLE_ID_LEN
        || !bundle.contains('.')
        || bundle
            .split('.')
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    {
        return Err(format!("некорректный bundle ID: {bundle}"));
    }
    Ok(())
}

fn parse_config_bytes(bytes: &[u8]) -> Result<Config, String> {
    let raw: RawConfig =
        plist::from_bytes(bytes).map_err(|e| format!("не удалось разобрать config.plist: {e}"))?;
    normalize_config(raw)
}

fn load_config(path: &Path) -> Result<Config, String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|_| "не удалось проверить config.plist".to_string())?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err("config.plist должен быть обычным файлом".into());
    }
    let bytes =
        fs::read(path).map_err(|_| "не удалось прочитать config.plist".to_string())?;
    parse_config_bytes(&bytes)
}

fn normalize_config(raw: RawConfig) -> Result<Config, String> {
    let team = raw.team.trim().to_string();
    let device = raw.device.trim().to_string();
    if team.is_empty() || device.is_empty() {
        return Err("в config.plist не указаны team или device".into());
    }
    let renew_before_hours = raw.renew_before_hours.unwrap_or(DEFAULT_RENEW_BEFORE_HOURS);
    if !(MIN_RENEW_BEFORE_HOURS..=MAX_RENEW_BEFORE_HOURS).contains(&renew_before_hours) {
        return Err(format!(
            "renew_before_hours должен быть в диапазоне {MIN_RENEW_BEFORE_HOURS}..={MAX_RENEW_BEFORE_HOURS}"
        ));
    }

    let mut apps: Vec<TrackedApp> = Vec::new();
    let mut push_entry = |bundle: String, name: Option<String>| -> Result<(), String> {
        let bundle = bundle.trim().to_string();
        validate_bundle_id(&bundle)?;
        let clean_name = name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        if let Some(existing) = apps.iter_mut().find(|a| a.bundle == bundle) {
            if existing.name.is_none() && clean_name.is_some() {
                existing.name = clean_name;
            }
            return Ok(());
        }
        apps.push(TrackedApp {
            bundle,
            name: clean_name,
        });
        Ok(())
    };

    if let Some(bundle) = raw.bundle {
        push_entry(bundle, raw.name)?;
    }
    if let Some(entries) = raw.bundles {
        for entry in entries {
            match entry {
                BundleEntry::Id(b) => push_entry(b, None)?,
                BundleEntry::Detailed { bundle, name } => push_entry(bundle, name)?,
            }
        }
    }
    if apps.is_empty() {
        return Err("в config.plist не указано ни одного приложения (bundle / bundles)".into());
    }

    Ok(Config {
        team,
        device,
        apps,
        renew_before_hours,
    })
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn format_timestamp_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant civil-from-days algorithm (UTC).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if month <= 2 { 1 } else { 0 };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn inspect_system(base: &Path, daemon_plist: &Path) -> SystemState {
    let config_path = base.join("config.plist");
    let config_exists = fs::symlink_metadata(&config_path)
        .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file());

    let (apple_session_exists, apple_session_valid, apple_session_error) =
        inspect_apple_session(base);

    let daemon_installed =
        fs::metadata(daemon_plist).is_ok_and(|m| m.is_file());
    let daemon_enabled = if daemon_installed {
        plist::from_file::<_, plist::Dictionary>(daemon_plist)
            .ok()
            .is_some_and(|dict| {
                dict.get("Disabled")
                    .and_then(plist::Value::as_boolean)
                    != Some(true)
            })
    } else {
        false
    };

    SystemState {
        config_exists,
        apple_session_exists,
        apple_session_valid,
        apple_session_error,
        daemon_installed,
        daemon_enabled,
    }
}

fn inspect_apple_session(base: &Path) -> (bool, bool, Option<String>) {
    let dir_meta = match fs::symlink_metadata(base) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return (
                false,
                false,
                Some("каталог сессии /var/mobile/Library/Rebox не создан".into()),
            );
        }
        Err(_) => {
            return (
                false,
                false,
                Some("не удалось проверить каталог сессии".into()),
            );
        }
    };
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return (
            false,
            false,
            Some("каталог Rebox должен быть обычной директорией".into()),
        );
    }
    let dir_private = dir_meta.permissions().mode() & 0o077 == 0;

    let session_path = base.join("apple-session.plist");
    let session_meta = match fs::symlink_metadata(&session_path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return (
                false,
                false,
                Some("сессия Apple ID не найдена (apple-session.plist отсутствует)".into()),
            );
        }
        Err(_) => {
            return (
                false,
                false,
                Some("не удалось проверить apple-session.plist".into()),
            );
        }
    };
    if !dir_private {
        return (
            true,
            false,
            Some("каталог Rebox должен иметь права 0700".into()),
        );
    }
    if session_meta.file_type().is_symlink() || !session_meta.is_file() {
        return (
            true,
            false,
            Some("apple-session.plist должен быть обычным файлом".into()),
        );
    }
    if session_meta.permissions().mode() & 0o077 != 0 {
        return (
            true,
            false,
            Some("apple-session.plist должен иметь права 0600".into()),
        );
    }
    match plist::from_file::<_, (String, plist::Dictionary)>(&session_path) {
        Ok((email, spd)) if !email.trim().is_empty() && !spd.is_empty() => (true, true, None),
        _ => (
            true,
            false,
            Some("файл apple-session.plist повреждён или неполон; выполните вход заново".into()),
        ),
    }
}

fn ensure_private_base_dir(base: &Path, create_if_missing: bool) -> Result<(), String> {
    match fs::symlink_metadata(base) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err("рабочий каталог Rebox должен быть обычной директорией".into());
            }
            if meta.permissions().mode() & 0o077 != 0 {
                return Err("рабочий каталог Rebox должен иметь права 0700".into());
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound && create_if_missing => {
            match fs::DirBuilder::new().mode(0o700).create(base) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("не удалось создать рабочий каталог Rebox".into()),
            }
            let meta = fs::symlink_metadata(base)
                .map_err(|_| "не удалось проверить рабочий каталог Rebox".to_string())?;
            if meta.file_type().is_symlink()
                || !meta.is_dir()
                || meta.permissions().mode() & 0o077 != 0
            {
                return Err("рабочий каталог Rebox должен иметь права 0700".into());
            }
            Ok(())
        }
        Err(_) => Err("рабочий каталог Rebox недоступен".into()),
    }
}

/// Non-destructively copy allowlisted config/session files from legacy
/// `/var/mobile/Library/DopamineRenew` to `target_base` if missing there.
fn migrate_legacy_data(legacy_base: &Path, target_base: &Path) -> Result<usize, String> {
    let legacy_meta = match fs::symlink_metadata(legacy_base) {
        Ok(meta) => meta,
        Err(_) => return Ok(0),
    };
    if legacy_meta.file_type().is_symlink() || !legacy_meta.is_dir() {
        return Ok(0);
    }

    let mut pending: Vec<(&str, PathBuf)> = Vec::new();
    for &name in LEGACY_MIGRATION_ALLOWLIST {
        let src = legacy_base.join(name);
        let Ok(src_meta) = fs::symlink_metadata(&src) else {
            continue;
        };
        if src_meta.file_type().is_symlink()
            || !src_meta.is_file()
            || src_meta.len() == 0
            || src_meta.len() > MAX_LEGACY_FILE_BYTES
        {
            continue;
        }
        let dest = target_base.join(name);
        match fs::symlink_metadata(&dest) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => pending.push((name, src)),
            _ => continue,
        }
    }

    if pending.is_empty() {
        return Ok(0);
    }

    ensure_private_base_dir(target_base, true)?;

    let mut migrated = 0usize;
    for (name, src) in pending {
        let Ok(bytes) = fs::read(&src) else {
            continue;
        };
        if bytes.is_empty() || bytes.len() as u64 > MAX_LEGACY_FILE_BYTES {
            continue;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = target_base.join(format!(
            ".{name}.migrate-{}-{nanos}-{seq}",
            std::process::id()
        ));
        let dest = target_base.join(name);
        let write_res = (|| -> io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, &dest)?;
            Ok(())
        })();
        if write_res.is_ok() {
            migrated += 1;
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }
    Ok(migrated)
}

struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn create_private_temp_dir(parent: &Path) -> Result<TempDir, String> {
    let pid = std::process::id();
    for _ in 0..16 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(format!("run-{pid}-{nanos}-{seq}"));
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(TempDir(dir)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("не удалось создать временный каталог".into()),
        }
    }
    Err("не удалось создать уникальный временный каталог".into())
}

fn create_status_temp_dir(base: &Path) -> Result<TempDir, String> {
    if let Ok(meta) = fs::symlink_metadata(base) {
        if meta.file_type().is_symlink() {
            return Err("рабочий каталог Rebox не должен быть символической ссылкой".into());
        }
        if meta.is_dir() {
            if let Ok(temp) = create_private_temp_dir(base) {
                return Ok(temp);
            }
        }
    }
    create_private_temp_dir(&env::temp_dir())
}

#[derive(Debug)]
enum LockError {
    Busy,
    Other(String),
}

fn acquire_lock(base: &Path) -> Result<fs::File, LockError> {
    ensure_private_base_dir(base, true).map_err(LockError::Other)?;
    let lock_path = base.join("controller.lock");
    if let Ok(meta) = fs::symlink_metadata(&lock_path) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(LockError::Other(
                "controller.lock должен быть обычным файлом".into(),
            ));
        }
    }
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|_| LockError::Other("не удалось открыть файл блокировки".into()))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(fs::TryLockError::WouldBlock) => Err(LockError::Busy),
        Err(_) => Err(LockError::Other("не удалось установить блокировку".into())),
    }
}

fn invoke(program: &Path, args: &[&OsStr]) -> Result<Vec<u8>, String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            format!(
                "не удалось запустить {}: {e}",
                program
                    .file_name()
                    .unwrap_or_else(|| OsStr::new("helper"))
                    .to_string_lossy()
            )
        })?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        let detail = reason.trim();
        let name = program
            .file_name()
            .unwrap_or_else(|| OsStr::new("helper"))
            .to_string_lossy();
        if detail.is_empty() {
            return Err(format!("{name}: завершился с кодом {}", output.status));
        }
        return Err(format!("{name}: {detail}"));
    }
    Ok(output.stdout)
}

fn payload(tool: &Path, file: &Path) -> Result<plist::Dictionary, String> {
    let bytes = invoke(tool, &[OsStr::new("payload"), file.as_os_str()])?;
    plist::from_bytes(&bytes).map_err(|_| "не удалось декодировать plist профиля".into())
}

fn expiration(profile: &plist::Dictionary) -> Result<u64, String> {
    let d = profile
        .get("ExpirationDate")
        .and_then(plist::Value::as_date)
        .ok_or("в профиле отсутствует дата истечения")?;
    SystemTime::from(d)
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| "некорректная дата истечения профиля".into())
}

fn entries<'a>(profile: &'a plist::Dictionary, key: &str) -> Result<&'a [plist::Value], String> {
    profile
        .get(key)
        .and_then(plist::Value::as_array)
        .map(Vec::as_slice)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("в профиле отсутствует {key}"))
}

// iOS stores certificate SHA-256 fingerprints in the provisioning database;
// Apple downloads carry the full DER certificates. Compare their identities.
fn certificate_fingerprints(profile: &plist::Dictionary) -> Result<HashSet<[u8; 32]>, String> {
    let mut fingerprints = HashSet::new();
    for cert in entries(profile, "DeveloperCertificates")? {
        let data = cert.as_data().filter(|b| !b.is_empty()).ok_or("некорректный сертификат профиля")?;
        let fingerprint: [u8; 32] = if data.len() == 32 {
            data.try_into().map_err(|_| "некорректный отпечаток сертификата")?
        } else {
            Sha256::digest(data).into()
        };
        fingerprints.insert(fingerprint);
    }
    Ok(fingerprints)
}

fn same_installed_profile(expected: &plist::Dictionary, installed: &plist::Dictionary) -> bool {
    let keys = ["UUID", "ExpirationDate", "Entitlements", "TeamIdentifier", "ProvisionedDevices"];
    keys.iter().all(|key| expected.get(*key).is_some() && expected.get(*key) == installed.get(*key))
        && matches!((certificate_fingerprints(expected), certificate_fingerprints(installed)), (Ok(a), Ok(b)) if a == b)
}

fn compatible(
    old: &plist::Dictionary,
    new: &plist::Dictionary,
    team: &str,
    bundle: &str,
    device: &str,
    now: u64,
) -> Result<u64, String> {
    fn ent(p: &plist::Dictionary) -> Result<&plist::Dictionary, String> {
        p.get("Entitlements")
            .and_then(plist::Value::as_dictionary)
            .ok_or_else(|| "в профиле отсутствуют Entitlements".to_string())
    }
    let old_ent = ent(old)?;
    let new_ent = ent(new)?;
    let app = old_ent
        .get("application-identifier")
        .and_then(plist::Value::as_string)
        .ok_or("в профиле отсутствует application-identifier")?;
    if app.split_once('.').map(|(_, id)| id) != Some(bundle)
        || app.contains('*')
        || old_ent != new_ent
    {
        return Err("идентификатор приложения или entitlements не совпадают".into());
    }
    let teams = entries(old, "TeamIdentifier")?;
    if teams != entries(new, "TeamIdentifier")?
        || !teams.iter().any(|x| x.as_string() == Some(team))
    {
        return Err("команда разработчика (TeamIdentifier) не совпадает".into());
    }
    for p in [old, new] {
        if !entries(p, "ProvisionedDevices")?.iter().any(|x| {
            x.as_string()
                .is_some_and(|id| id.eq_ignore_ascii_case(device))
        }) {
            return Err("профиль не включает этот iPhone".into());
        }
    }
    let old_certs = certificate_fingerprints(old)?;
    let new_certs = certificate_fingerprints(new)?;
    if !old_certs.is_subset(&new_certs) {
        return Err("сертификат подписи отличается".into());
    }
    let expires = expiration(new)?;
    if expires <= expiration(old)? || expires <= now {
        return Err("Apple не продлила срок действия профиля".into());
    }
    Ok(expires)
}

fn looks_like_team_suffix(segment: &str, team: &str) -> bool {
    if !team.is_empty() && segment.eq_ignore_ascii_case(team) {
        return true;
    }
    segment.len() == 10
        && segment
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn derive_name_from_bundle(bundle: &str, team: &str) -> String {
    let parts: Vec<&str> = bundle.split('.').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 2 && looks_like_team_suffix(parts[parts.len() - 1], team) {
        return parts[parts.len() - 2].to_string();
    }
    parts
        .last()
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| bundle.to_string())
}

fn resolve_app_display_name(
    configured_name: Option<&str>,
    profile: Option<&plist::Dictionary>,
    bundle: &str,
    team: &str,
) -> String {
    if let Some(name) = configured_name.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    if let Some(dict) = profile {
        if let Some(app_id_name) = dict
            .get("AppIDName")
            .and_then(plist::Value::as_string)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !app_id_name.starts_with("XC ") {
                return app_id_name.to_string();
            }
        }
    }
    derive_name_from_bundle(bundle, team)
}

fn app_status_from_expiry(
    name: String,
    bundle_id: String,
    expires_at_unix: u64,
    now: u64,
    renew_before_hours: u64,
    renewed: bool,
) -> AppStatus {
    let threshold = now.saturating_add(renew_before_hours.saturating_mul(3600));
    if expires_at_unix <= now {
        AppStatus {
            name,
            bundle_id,
            status: "expired".to_string(),
            expires_at_unix: Some(expires_at_unix),
            hours_remaining: Some(0),
            needs_renewal: true,
            renewed,
            message: "Срок подписи истёк".to_string(),
        }
    } else {
        let remaining_secs = expires_at_unix - now;
        let hours = remaining_secs / 3600;
        let needs_renewal = expires_at_unix <= threshold;
        let status = if needs_renewal {
            "expiring_soon"
        } else {
            "ok"
        };
        let message = if renewed {
            format!("Подпись продлена (~{hours} ч)")
        } else if needs_renewal {
            if hours == 0 {
                "Скоро истечёт: осталось <1 ч".to_string()
            } else {
                format!("Скоро истечёт: осталось ~{hours} ч")
            }
        } else {
            format!("Действует ещё ~{hours} ч")
        };
        AppStatus {
            name,
            bundle_id,
            status: status.to_string(),
            expires_at_unix: Some(expires_at_unix),
            hours_remaining: Some(hours),
            needs_renewal,
            renewed,
            message,
        }
    }
}

fn compute_overall_status(
    sys: &SystemState,
    apps: &[AppStatus],
    fatal_error: Option<&str>,
) -> String {
    if !sys.config_exists {
        return "needs_setup".to_string();
    }
    if fatal_error.is_some() || apps.iter().any(|a| a.status == "error") {
        return "error".to_string();
    }
    if apps.iter().any(|a| a.status == "expired") {
        return "expired".to_string();
    }
    if apps.iter().any(|a| a.status == "expiring_soon") {
        return "expiring_soon".to_string();
    }
    if !sys.apple_session_valid {
        return "needs_setup".to_string();
    }
    "ok".to_string()
}

fn summarize_report_message(
    command: &str,
    overall_status: &str,
    sys: &SystemState,
    apps: &[AppStatus],
    last_error: Option<&str>,
) -> String {
    if !sys.config_exists {
        return "Требуется настройка: создайте /var/mobile/Library/Rebox/config.plist (через setup-device.sh или init-from-installed) и авторизуйте Apple ID.".to_string();
    }
    if overall_status == "needs_setup" && !sys.apple_session_valid {
        return sys
            .apple_session_error
            .clone()
            .unwrap_or_else(|| "Требуется вход в Apple ID (apple-session.plist отсутствует или повреждён).".to_string());
    }
    if let Some(err) = last_error {
        return err.to_string();
    }
    let renewed_count = apps.iter().filter(|a| a.renewed).count();
    if (command == "refresh" || command == "auto") && renewed_count > 0 {
        if renewed_count == 1 {
            if let Some(app) = apps.iter().find(|a| a.renewed) {
                return format!(
                    "Подпись {} продлена (~{} ч).",
                    app.name,
                    app.hours_remaining.unwrap_or(0)
                );
            }
        }
        return format!("Продлено подписей: {renewed_count}.");
    }
    match overall_status {
        "ok" => {
            if apps.len() == 1 {
                format!("{}: {}.", apps[0].name, apps[0].message)
            } else {
                format!("Все подписи действуют ({} прил.).", apps.len())
            }
        }
        "expiring_soon" => "Есть подписи, которые скоро истекут.".to_string(),
        "expired" => "Срок действия одной или нескольких подписей истёк.".to_string(),
        _ => "Проверка завершена с ошибкой.".to_string(),
    }
}

fn build_report(
    command: &str,
    sys: &SystemState,
    renew_before_hours: u64,
    now: u64,
    apps: Vec<AppStatus>,
    last_error: Option<String>,
    override_status: Option<&str>,
) -> ControllerReport {
    let status = override_status
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| compute_overall_status(sys, &apps, last_error.as_deref()));
    let message = summarize_report_message(command, &status, sys, &apps, last_error.as_deref());
    ControllerReport {
        schema_version: 1,
        command: command.to_string(),
        status,
        message,
        config_exists: sys.config_exists,
        apple_session_exists: sys.apple_session_exists,
        apple_session_valid: sys.apple_session_valid,
        daemon_installed: sys.daemon_installed,
        daemon_enabled: sys.daemon_enabled,
        renew_before_hours,
        checked_at_unix: now,
        apps,
        last_error,
    }
}

fn inspect_tracked_app(
    tool: &Path,
    temp_dir: &Path,
    idx: usize,
    app: &TrackedApp,
    config: &Config,
    now: u64,
) -> (AppStatus, Option<(PathBuf, plist::Dictionary, u64)>) {
    let old_path = temp_dir.join(format!("old-{idx}.mobileprovision"));
    if let Err(err) = invoke(
        tool,
        &[
            OsStr::new("export"),
            OsStr::new(&app.bundle),
            old_path.as_os_str(),
        ],
    ) {
        let fallback_name = resolve_app_display_name(app.name.as_deref(), None, &app.bundle, &config.team);
        return (
            AppStatus {
                name: fallback_name,
                bundle_id: app.bundle.clone(),
                status: "error".to_string(),
                expires_at_unix: None,
                hours_remaining: None,
                needs_renewal: true,
                renewed: false,
                message: format!("Не удалось экспортировать профиль: {err}"),
            },
            None,
        );
    }

    let old = match payload(tool, &old_path) {
        Ok(dict) => dict,
        Err(err) => {
            let fallback_name =
                resolve_app_display_name(app.name.as_deref(), None, &app.bundle, &config.team);
            return (
                AppStatus {
                    name: fallback_name,
                    bundle_id: app.bundle.clone(),
                    status: "error".to_string(),
                    expires_at_unix: None,
                    hours_remaining: None,
                    needs_renewal: true,
                    renewed: false,
                    message: format!("Не удалось прочитать профиль: {err}"),
                },
                None,
            );
        }
    };

    let name = resolve_app_display_name(app.name.as_deref(), Some(&old), &app.bundle, &config.team);
    let old_expiry = match expiration(&old) {
        Ok(exp) => exp,
        Err(err) => {
            return (
                AppStatus {
                    name,
                    bundle_id: app.bundle.clone(),
                    status: "error".to_string(),
                    expires_at_unix: None,
                    hours_remaining: None,
                    needs_renewal: true,
                    renewed: false,
                    message: err,
                },
                None,
            );
        }
    };

    let status = app_status_from_expiry(
        name,
        app.bundle.clone(),
        old_expiry,
        now,
        config.renew_before_hours,
        false,
    );
    (status, Some((old_path, old, old_expiry)))
}

fn renew_tracked_app(
    tool: &Path,
    apple: &Path,
    base: &Path,
    temp_dir: &Path,
    idx: usize,
    app: &TrackedApp,
    config: &Config,
    old: &plist::Dictionary,
    display_name: String,
    now: u64,
) -> Result<AppStatus, AppStatus> {
    let fail = |msg: String| AppStatus {
        name: display_name.clone(),
        bundle_id: app.bundle.clone(),
        status: "error".to_string(),
        expires_at_unix: expiration(old).ok(),
        hours_remaining: expiration(old).ok().map(|e| e.saturating_sub(now) / 3600),
        needs_renewal: true,
        renewed: false,
        message: msg,
    };

    let new_path = temp_dir.join(format!("new-{idx}.mobileprovision"));
    if let Err(err) = invoke(
        apple,
        &[
            OsStr::new("fetch"),
            base.as_os_str(),
            OsStr::new(&config.team),
            OsStr::new(&app.bundle),
            new_path.as_os_str(),
        ],
    ) {
        return Err(fail(format!("Ошибка загрузки профиля у Apple: {err}")));
    }

    let new_meta = fs::symlink_metadata(&new_path)
        .map_err(|_| fail("новый профиль недоступен".to_string()))?;
    if new_meta.file_type().is_symlink()
        || !new_meta.is_file()
        || new_meta.permissions().mode() & 0o077 != 0
    {
        return Err(fail("новый профиль не имеет приватных прав 0600".to_string()));
    }

    let new = payload(tool, &new_path).map_err(|e| fail(format!("не удалось декодировать новый профиль: {e}")))?;
    let expiry = compatible(old, &new, &config.team, &app.bundle, &config.device, now)
        .map_err(|e| fail(format!("проверка совместимости не пройдена: {e}")))?;

    invoke(tool, &[OsStr::new("install"), new_path.as_os_str()])
        .map_err(|e| fail(format!("не удалось установить профиль: {e}")))?;

    let checked_path = temp_dir.join(format!("check-{idx}.mobileprovision"));
    invoke(
        tool,
        &[
            OsStr::new("export"),
            OsStr::new(&app.bundle),
            checked_path.as_os_str(),
        ],
    )
    .map_err(|e| fail(format!("не удалось проверить установленный профиль: {e}")))?;

    let installed = payload(tool, &checked_path)
        .map_err(|e| fail(format!("не удалось декодировать установленный профиль: {e}")))?;
    let installed_expiry = compatible(old, &installed, &config.team, &app.bundle, &config.device, now)
        .map_err(|e| fail(format!("установленный профиль не прошёл проверку: {e}")))?;
    if installed_expiry != expiry || !same_installed_profile(&new, &installed) {
        return Err(fail("iOS не подтвердила замену профиля".to_string()));
    }

    let after_now = now_unix().max(now);
    Ok(app_status_from_expiry(
        display_name,
        app.bundle.clone(),
        installed_expiry,
        after_now,
        config.renew_before_hours,
        true,
    ))
}

fn execute_status(rt: &RuntimeEnv) -> (ControllerReport, ExitCode) {
    let now = now_unix();
    let sys = inspect_system(&rt.base, &rt.daemon_plist);
    if !sys.config_exists {
        return (
            build_report(
                "status",
                &sys,
                DEFAULT_RENEW_BEFORE_HOURS,
                now,
                Vec::new(),
                None,
                Some("needs_setup"),
            ),
            ExitCode::SUCCESS,
        );
    }

    let config = match load_config(&rt.base.join("config.plist")) {
        Ok(c) => c,
        Err(err) => {
            return (
                build_report(
                    "status",
                    &sys,
                    DEFAULT_RENEW_BEFORE_HOURS,
                    now,
                    Vec::new(),
                    Some(err),
                    Some("error"),
                ),
                ExitCode::SUCCESS,
            );
        }
    };

    let Some(tool) = resolve_helper_in("profilectl", &rt.helper_dirs) else {
        return (
            build_report(
                "status",
                &sys,
                config.renew_before_hours,
                now,
                Vec::new(),
                Some("Не найден вспомогательный модуль profilectl.".to_string()),
                Some("error"),
            ),
            ExitCode::FAILURE,
        );
    };

    let temp = match create_status_temp_dir(&rt.base) {
        Ok(t) => t,
        Err(err) => {
            return (
                build_report(
                    "status",
                    &sys,
                    config.renew_before_hours,
                    now,
                    Vec::new(),
                    Some(err),
                    Some("error"),
                ),
                ExitCode::FAILURE,
            );
        }
    };

    let mut apps = Vec::with_capacity(config.apps.len());
    let mut first_error = None;
    for (idx, app) in config.apps.iter().enumerate() {
        let (st, _) = inspect_tracked_app(&tool, &temp.0, idx, app, &config, now);
        if st.status == "error" && first_error.is_none() {
            first_error = Some(format!("{}: {}", st.name, st.message));
        }
        apps.push(st);
    }

    let report = build_report(
        "status",
        &sys,
        config.renew_before_hours,
        now,
        apps,
        first_error,
        None,
    );
    (report, ExitCode::SUCCESS)
}

fn execute_renew(
    rt: &RuntimeEnv,
    mode: &str,
    target_bundle: Option<&str>,
) -> (ControllerReport, ExitCode) {
    let now = now_unix();
    let sys = inspect_system(&rt.base, &rt.daemon_plist);
    if !sys.config_exists {
        let err = "Конфигурация /var/mobile/Library/Rebox/config.plist не найдена.".to_string();
        return (
            build_report(
                mode,
                &sys,
                DEFAULT_RENEW_BEFORE_HOURS,
                now,
                Vec::new(),
                Some(err),
                Some("needs_setup"),
            ),
            ExitCode::FAILURE,
        );
    }

    let config = match load_config(&rt.base.join("config.plist")) {
        Ok(c) => c,
        Err(err) => {
            return (
                build_report(
                    mode,
                    &sys,
                    DEFAULT_RENEW_BEFORE_HOURS,
                    now,
                    Vec::new(),
                    Some(err),
                    Some("error"),
                ),
                ExitCode::FAILURE,
            );
        }
    };

    let selected_apps: Vec<&TrackedApp> = if let Some(target) = target_bundle {
        let found: Vec<&TrackedApp> = config.apps.iter().filter(|a| a.bundle == target).collect();
        if found.is_empty() {
            return (
                build_report(
                    mode,
                    &sys,
                    config.renew_before_hours,
                    now,
                    Vec::new(),
                    Some(format!("Приложение {target} не найдено в config.plist.")),
                    Some("error"),
                ),
                ExitCode::FAILURE,
            );
        }
        found
    } else {
        config.apps.iter().collect()
    };

    let _lock = match acquire_lock(&rt.base) {
        Ok(l) => l,
        Err(LockError::Busy) => {
            return (
                build_report(
                    mode,
                    &sys,
                    config.renew_before_hours,
                    now,
                    Vec::new(),
                    Some("Операция продления уже выполняется.".to_string()),
                    Some("error"),
                ),
                ExitCode::FAILURE,
            );
        }
        Err(LockError::Other(err)) => {
            return (
                build_report(
                    mode,
                    &sys,
                    config.renew_before_hours,
                    now,
                    Vec::new(),
                    Some(err),
                    Some("error"),
                ),
                ExitCode::FAILURE,
            );
        }
    };

    let Some(tool) = resolve_helper_in("profilectl", &rt.helper_dirs) else {
        return (
            build_report(
                mode,
                &sys,
                config.renew_before_hours,
                now,
                Vec::new(),
                Some("Не найден вспомогательный модуль profilectl.".to_string()),
                Some("error"),
            ),
            ExitCode::FAILURE,
        );
    };

    let temp = match create_private_temp_dir(&rt.base) {
        Ok(t) => t,
        Err(err) => {
            return (
                build_report(
                    mode,
                    &sys,
                    config.renew_before_hours,
                    now,
                    Vec::new(),
                    Some(err),
                    Some("error"),
                ),
                ExitCode::FAILURE,
            );
        }
    };

    let apple_bin = resolve_helper_in("rebox-apple", &rt.helper_dirs);
    let mut apps = Vec::with_capacity(selected_apps.len());
    let mut first_error: Option<String> = None;
    let mut any_failed = false;

    for (idx, app) in selected_apps.iter().enumerate() {
        let (inspected, maybe_old) = inspect_tracked_app(&tool, &temp.0, idx, app, &config, now);
        let Some((_old_path, old_dict, _old_expiry)) = maybe_old else {
            any_failed = true;
            if first_error.is_none() {
                first_error = Some(format!("{}: {}", inspected.name, inspected.message));
            }
            apps.push(inspected);
            continue;
        };

        let should_renew = mode == "refresh" || inspected.needs_renewal;
        if !should_renew {
            apps.push(inspected);
            continue;
        }

        if !sys.apple_session_valid {
            any_failed = true;
            let msg = sys
                .apple_session_error
                .clone()
                .unwrap_or_else(|| "Сессия Apple ID не настроена.".to_string());
            if first_error.is_none() {
                first_error = Some(msg);
            }
            apps.push(inspected);
            continue;
        }

        let Some(apple) = apple_bin.as_deref() else {
            any_failed = true;
            let msg = "Не найден вспомогательный модуль rebox-apple.".to_string();
            if first_error.is_none() {
                first_error = Some(msg.clone());
            }
            apps.push(AppStatus {
                status: "error".to_string(),
                message: msg,
                ..inspected
            });
            continue;
        };

        match renew_tracked_app(
            &tool,
            apple,
            &rt.base,
            &temp.0,
            idx,
            app,
            &config,
            &old_dict,
            inspected.name,
            now,
        ) {
            Ok(renewed_status) => apps.push(renewed_status),
            Err(failed_status) => {
                any_failed = true;
                if first_error.is_none() {
                    first_error = Some(format!("{}: {}", failed_status.name, failed_status.message));
                }
                apps.push(failed_status);
            }
        }
    }

    if !sys.apple_session_valid && first_error.is_none() {
        let msg = sys
            .apple_session_error
            .clone()
            .unwrap_or_else(|| "Сессия Apple ID не настроена.".to_string());
        let report = build_report(
            mode,
            &sys,
            config.renew_before_hours,
            now,
            apps,
            Some(msg),
            Some("needs_setup"),
        );
        return (report, ExitCode::FAILURE);
    }

    let override_status = if any_failed && !sys.apple_session_valid {
        Some("needs_setup")
    } else if any_failed {
        Some("error")
    } else {
        None
    };

    let report = build_report(
        mode,
        &sys,
        config.renew_before_hours,
        now,
        apps,
        first_error,
        override_status,
    );
    let code = if any_failed || report.status == "needs_setup" || report.status == "error" {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    };
    (report, code)
}

fn format_report_human(report: &ControllerReport, timestamped: bool) -> String {
    let prefix = if timestamped {
        format!("[{}] ", format_timestamp_utc(report.checked_at_unix))
    } else {
        String::new()
    };

    let mut lines = Vec::new();
    if !report.config_exists {
        lines.push(format!("{prefix}{}", report.message));
        lines.push(format!(
            "{prefix}Сессия Apple ID: {} | Фоновая служба: {}",
            if report.apple_session_valid {
                "активна"
            } else {
                "требуется вход"
            },
            if report.daemon_enabled {
                "включена"
            } else if report.daemon_installed {
                "отключена"
            } else {
                "не установлена"
            }
        ));
        return lines.join("\n");
    }

    for app in &report.apps {
        lines.push(format!(
            "{prefix}{} ({}): {}",
            app.name, app.bundle_id, app.message
        ));
    }

    if report.apps.is_empty() || report.last_error.is_some() || !report.apple_session_valid {
        lines.push(format!("{prefix}{}", report.message));
    }

    if !timestamped {
        lines.push(format!(
            "Сессия Apple ID: {} | Фоновая служба: {} | Окно продления: {} ч",
            if report.apple_session_valid {
                "активна"
            } else {
                "требуется вход"
            },
            if report.daemon_enabled {
                "включена"
            } else if report.daemon_installed {
                "отключена"
            } else {
                "не установлена"
            },
            report.renew_before_hours
        ));
    }

    lines.join("\n")
}

fn run_discover(rt: &RuntimeEnv, json: bool) -> ExitCode {
    let Some(tool) = resolve_helper_in("profilectl", &rt.helper_dirs) else {
        if json {
            println!(r#"{{"error":"profilectl not found","profiles":[]}}"#);
        } else {
            eprintln!("Не найден вспомогательный модуль profilectl.");
        }
        return ExitCode::FAILURE;
    };
    let flag = if json { "--json" } else { "--human" };
    match invoke(&tool, &[OsStr::new("list"), OsStr::new(flag)]) {
        Ok(bytes) => {
            let _ = io::stdout().write_all(&bytes);
            ExitCode::SUCCESS
        }
        Err(err) => {
            if json {
                let fallback = serde_json::json!({
                    "error": err,
                    "profiles": []
                });
                println!("{fallback}");
            } else {
                eprintln!("Не удалось получить список профилей: {err}");
            }
            ExitCode::FAILURE
        }
    }
}

fn build_config_from_discovered(
    profiles: &[DiscoveredProfile],
    requested_bundles: &[String],
    device_override: Option<&str>,
    team_override: Option<&str>,
    renew_before_hours: u64,
) -> Result<plist::Dictionary, String> {
    if !(MIN_RENEW_BEFORE_HOURS..=MAX_RENEW_BEFORE_HOURS).contains(&renew_before_hours) {
        return Err("некорректное значение renew_before_hours".into());
    }
    let mut selected: Vec<&DiscoveredProfile> = Vec::new();
    let mut seen = HashSet::new();
    for p in profiles {
        if p.is_wildcard || validate_bundle_id(&p.bundle_id).is_err() {
            continue;
        }
        if !requested_bundles.is_empty() && !requested_bundles.iter().any(|b| b == &p.bundle_id) {
            continue;
        }
        if seen.insert(p.bundle_id.clone()) {
            selected.push(p);
        }
    }
    if selected.is_empty() {
        return Err("подходящие установленные профили не найдены".into());
    }

    let team = if let Some(t) = team_override.map(str::trim).filter(|s| !s.is_empty()) {
        t.to_string()
    } else {
        let teams: HashSet<&str> = selected
            .iter()
            .map(|p| p.team_id.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        if teams.len() != 1 {
            return Err("найдено несколько Team ID; укажите --team TEAM_ID".into());
        }
        teams.into_iter().next().unwrap().to_string()
    };

    let device = if let Some(d) = device_override.map(str::trim).filter(|s| !s.is_empty()) {
        d.to_string()
    } else {
        let mut common: Option<HashSet<String>> = None;
        for p in &selected {
            let devs: HashSet<String> = p.provisioned_devices.iter().cloned().collect();
            common = Some(match common {
                None => devs,
                Some(prev) => prev.intersection(&devs).cloned().collect(),
            });
        }
        let devs = common.unwrap_or_default();
        if devs.len() != 1 {
            return Err("не удалось однозначно определить UDID iPhone; укажите --device UDID".into());
        }
        devs.into_iter().next().unwrap()
    };

    let mut dict = plist::Dictionary::new();
    dict.insert("team".into(), plist::Value::String(team.clone()));
    dict.insert("device".into(), plist::Value::String(device));
    dict.insert(
        "renew_before_hours".into(),
        plist::Value::Integer(renew_before_hours.into()),
    );
    if selected.len() == 1 {
        let p = selected[0];
        dict.insert("bundle".into(), plist::Value::String(p.bundle_id.clone()));
        let display = if !p.app_name.trim().is_empty() && !p.app_name.starts_with("XC ") {
            p.app_name.trim().to_string()
        } else {
            derive_name_from_bundle(&p.bundle_id, &team)
        };
        dict.insert("name".into(), plist::Value::String(display));
    } else {
        let items: Vec<plist::Value> = selected
            .iter()
            .map(|p| {
                let mut item = plist::Dictionary::new();
                item.insert("bundle".into(), plist::Value::String(p.bundle_id.clone()));
                let display = if !p.app_name.trim().is_empty() && !p.app_name.starts_with("XC ") {
                    p.app_name.trim().to_string()
                } else {
                    derive_name_from_bundle(&p.bundle_id, &team)
                };
                item.insert("name".into(), plist::Value::String(display));
                plist::Value::Dictionary(item)
            })
            .collect();
        dict.insert("bundles".into(), plist::Value::Array(items));
    }
    Ok(dict)
}

fn run_init_from_installed(rt: &RuntimeEnv, args: &[String]) -> ExitCode {
    let mut device: Option<String> = None;
    let mut team: Option<String> = None;
    let mut bundles: Vec<String> = Vec::new();
    let mut force = false;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--device" if idx + 1 < args.len() => {
                device = Some(args[idx + 1].clone());
                idx += 2;
            }
            "--team" if idx + 1 < args.len() => {
                team = Some(args[idx + 1].clone());
                idx += 2;
            }
            "--bundle" if idx + 1 < args.len() => {
                bundles.push(args[idx + 1].clone());
                idx += 2;
            }
            "--force" => {
                force = true;
                idx += 1;
            }
            other => {
                eprintln!("Неизвестный аргумент для init-from-installed: {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    let _lock = match acquire_lock(&rt.base) {
        Ok(l) => l,
        Err(LockError::Busy) => {
            eprintln!("Операция продления уже выполняется.");
            return ExitCode::FAILURE;
        }
        Err(LockError::Other(e)) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let config_path = rt.base.join("config.plist");
    if fs::symlink_metadata(&config_path).is_ok() && !force {
        eprintln!(
            "Файл {} уже существует (используйте --force для перезаписи).",
            config_path.display()
        );
        return ExitCode::FAILURE;
    }

    let Some(tool) = resolve_helper_in("profilectl", &rt.helper_dirs) else {
        eprintln!("Не найден вспомогательный модуль profilectl.");
        return ExitCode::FAILURE;
    };
    let raw_json = match invoke(&tool, &[OsStr::new("list"), OsStr::new("--json")]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Не удалось получить список установленных профилей: {e}");
            return ExitCode::FAILURE;
        }
    };
    let profiles: Vec<DiscoveredProfile> = match serde_json::from_slice(&raw_json) {
        Ok(p) => p,
        Err(_) => {
            eprintln!("Не удалось разобрать список профилей от profilectl.");
            return ExitCode::FAILURE;
        }
    };
    let dict = match build_config_from_discovered(
        &profiles,
        &bundles,
        device.as_deref(),
        team.as_deref(),
        DEFAULT_RENEW_BEFORE_HOURS,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut xml = Vec::new();
    if plist::to_writer_xml(&mut xml, &plist::Value::Dictionary(dict)).is_err() {
        eprintln!("Не удалось сериализовать config.plist.");
        return ExitCode::FAILURE;
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = rt.base.join(format!(
        "config.plist.tmp.{}-{nanos}-{seq}",
        std::process::id()
    ));
    let write_res = (|| -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)?;
        f.write_all(&xml)?;
        f.sync_all()?;
        fs::rename(&tmp_path, &config_path)?;
        Ok(())
    })();
    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
        eprintln!("Не удалось сохранить config.plist.");
        return ExitCode::FAILURE;
    }
    println!("Конфигурация сохранена: {}", config_path.display());
    ExitCode::SUCCESS
}

fn emit_report(report: &ControllerReport, json: bool, timestamped: bool) {
    if json {
        match serde_json::to_string_pretty(report) {
            Ok(out) => println!("{out}"),
            Err(_) => println!(
                r#"{{"schema_version":1,"command":"status","status":"error","message":"Не удалось сериализовать JSON","config_exists":false,"apple_session_exists":false,"apple_session_valid":false,"daemon_installed":false,"daemon_enabled":false,"renew_before_hours":48,"checked_at_unix":0,"apps":[],"last_error":"serialization failed"}}"#
            ),
        }
    } else {
        println!("{}", format_report_human(report, timestamped));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedCli {
    Status { json: bool },
    Refresh { json: bool, bundle: Option<String> },
    Auto { json: bool },
    Discover { json: bool },
    InitFromInstalled { rest: Vec<String> },
}

fn parse_cli(args: &[String]) -> Result<ParsedCli, (bool, String)> {
    let json = args.iter().any(|a| a == "--json");
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| a.as_str() != "--json")
        .map(String::as_str)
        .collect();
    match positional.as_slice() {
        ["status"] => Ok(ParsedCli::Status { json }),
        ["refresh"] => Ok(ParsedCli::Refresh { json, bundle: None }),
        ["refresh", "--bundle", b] => {
            validate_bundle_id(b).map_err(|e| (json, e))?;
            Ok(ParsedCli::Refresh {
                json,
                bundle: Some((*b).to_string()),
            })
        }
        ["auto"] => Ok(ParsedCli::Auto { json }),
        ["discover"] => Ok(ParsedCli::Discover { json }),
        ["init-from-installed", rest @ ..] => Ok(ParsedCli::InitFromInstalled {
            rest: rest.iter().map(|s| (*s).to_string()).collect(),
        }),
        _ => Err((
            json,
            "Usage: rebox-controller status|refresh [--bundle BUNDLE]|auto|discover [--json] | init-from-installed [--device UDID] [--team TEAM] [--bundle BUNDLE] [--force]".to_string(),
        )),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let rt = RuntimeEnv::production();
    let parsed = parse_cli(&args);
    if matches!(
        parsed,
        Ok(ParsedCli::Status { .. }
            | ParsedCli::Refresh { .. }
            | ParsedCli::Auto { .. }
            | ParsedCli::InitFromInstalled { .. })
    ) {
        let _ = migrate_legacy_data(Path::new(LEGACY_BASE), &rt.base);
    }
    // Fail closed if a linked session cannot be synchronized safely.
    let sync_command = match &parsed {
        Ok(ParsedCli::Status { json }) => Some(("status", *json)),
        Ok(ParsedCli::Refresh { json, .. }) => Some(("refresh", *json)),
        Ok(ParsedCli::Auto { json }) => Some(("auto", *json)),
        _ => None,
    };
    if let Some((command, json)) = sync_command {
        if let Err(err) = sidebox_bridge::sync_production(&rt.base) {
            let system = inspect_system(&rt.base, &rt.daemon_plist);
            let report = build_report(command, &system, DEFAULT_RENEW_BEFORE_HOURS,
                now_unix(), Vec::new(), Some(err), Some("error"));
            emit_report(&report, json, false);
            return ExitCode::FAILURE;
        }
    }
    match parsed {
        Ok(ParsedCli::Status { json }) => {
            let (report, code) = execute_status(&rt);
            emit_report(&report, json, false);
            code
        }
        Ok(ParsedCli::Refresh { json, bundle }) => {
            let (report, code) = execute_renew(&rt, "refresh", bundle.as_deref());
            emit_report(&report, json, false);
            code
        }
        Ok(ParsedCli::Auto { json }) => {
            let (report, code) = execute_renew(&rt, "auto", None);
            emit_report(&report, json, !json);
            code
        }
        Ok(ParsedCli::Discover { json }) => run_discover(&rt, json),
        Ok(ParsedCli::InitFromInstalled { rest }) => run_init_from_installed(&rt, &rest),
        Err((json, msg)) => {
            if json {
                let sys = inspect_system(&rt.base, &rt.daemon_plist);
                let report = build_report(
                    "status",
                    &sys,
                    DEFAULT_RENEW_BEFORE_HOURS,
                    now_unix(),
                    Vec::new(),
                    Some(msg),
                    Some("error"),
                );
                emit_report(&report, true, false);
            } else {
                eprintln!("{msg}");
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn unique_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!(
            "rebox-controller-test-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn make_profile_dict(
        team: &str,
        bundle: &str,
        app_name: &str,
        device: &str,
        cert_bytes: &[u8],
        expires_unix: u64,
    ) -> plist::Dictionary {
        let mut ent = plist::Dictionary::new();
        ent.insert(
            "application-identifier".into(),
            plist::Value::String(format!("{team}.{bundle}")),
        );
        ent.insert(
            "get-task-allow".into(),
            plist::Value::Boolean(true),
        );

        let mut d = plist::Dictionary::new();
        // A real installed provisioning profile always has a stable UUID.
        // It must remain unchanged when the mock installs a renewed profile.
        d.insert("UUID".into(), plist::Value::String(format!("mock-{bundle}")));
        d.insert("AppIDName".into(), plist::Value::String(app_name.into()));
        d.insert(
            "TeamIdentifier".into(),
            plist::Value::Array(vec![plist::Value::String(team.into())]),
        );
        d.insert(
            "ProvisionedDevices".into(),
            plist::Value::Array(vec![plist::Value::String(device.into())]),
        );
        d.insert(
            "DeveloperCertificates".into(),
            plist::Value::Array(vec![plist::Value::Data(cert_bytes.to_vec())]),
        );
        d.insert(
            "ExpirationDate".into(),
            plist::Value::Date((UNIX_EPOCH + std::time::Duration::from_secs(expires_unix)).into()),
        );
        d.insert("Entitlements".into(), plist::Value::Dictionary(ent));
        d
    }

    fn write_xml_plist(path: &Path, dict: &plist::Dictionary, mode: u32) {
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &plist::Value::Dictionary(dict.clone())).unwrap();
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(mode)
            .open(path)
            .unwrap();
        f.write_all(&bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write_session(base: &Path, email: &str, token: &str) {
        let mut spd = plist::Dictionary::new();
        spd.insert("GSIdmsToken".into(), plist::Value::String(token.into()));
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &(email, spd)).unwrap();
        let path = base.join("apple-session.plist");
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        f.write_all(&bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn write_script(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn config_parsing_single_and_multi_bundles() {
        let single_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>team</key><string>TEAM123456</string>
  <key>bundle</key><string>com.SideStore.SideStore.TEAM123456</string>
  <key>device</key><string>00000000-0000AABBCCDDEEFF</string>
</dict></plist>"#;
        let cfg = parse_config_bytes(single_xml).unwrap();
        assert_eq!(cfg.team, "TEAM123456");
        assert_eq!(cfg.device, "00000000-0000AABBCCDDEEFF");
        assert_eq!(cfg.renew_before_hours, 48);
        assert_eq!(
            cfg.apps,
            vec![TrackedApp {
                bundle: "com.SideStore.SideStore.TEAM123456".into(),
                name: None
            }]
        );

        let multi_xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>team</key><string>TEAM123456</string>
  <key>device</key><string>00000000-0000AABBCCDDEEFF</string>
  <key>renew_before_hours</key><integer>24</integer>
  <key>bundle</key><string>com.SideStore.SideStore.TEAM123456</string>
  <key>bundles</key><array>
    <dict>
      <key>bundle</key><string>com.SideStore.SideStore.TEAM123456</string>
      <key>name</key><string>SideStore</string>
    </dict>
    <string>org.example.AppTwo</string>
  </array>
</dict></plist>"#;
        let cfg2 = parse_config_bytes(multi_xml).unwrap();
        assert_eq!(cfg2.renew_before_hours, 24);
        assert_eq!(cfg2.apps.len(), 2);
        assert_eq!(cfg2.apps[0].name.as_deref(), Some("SideStore"));
        assert_eq!(cfg2.apps[1].bundle, "org.example.AppTwo");

        let bad_hours = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>team</key><string>TEAM123456</string>
  <key>device</key><string>00000000-0000AABBCCDDEEFF</string>
  <key>bundle</key><string>com.SideStore.SideStore</string>
  <key>renew_before_hours</key><integer>0</integer>
</dict></plist>"#;
        assert!(parse_config_bytes(bad_hours).is_err());

        let bad_bundle = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>team</key><string>TEAM123456</string>
  <key>device</key><string>00000000-0000AABBCCDDEEFF</string>
  <key>bundle</key><string>not_a_bundle</string>
</dict></plist>"#;
        assert!(parse_config_bytes(bad_bundle).is_err());
    }

    #[test]
    fn compatibility_checks_all_security_invariants() {
        let now = 1_760_000_000;
        let old = make_profile_dict(
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "00000000-0000AABBCCDDEEFF",
            b"CERT-A",
            now + 10_000,
        );
        let good_new = make_profile_dict(
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "00000000-0000aabbccddeeff", // case-insensitive UDID match
            b"CERT-A",
            now + 600_000,
        );
        assert_eq!(
            compatible(
                &old,
                &good_new,
                "TEAM123456",
                "com.SideStore.SideStore.TEAM123456",
                "00000000-0000AABBCCDDEEFF",
                now
            )
            .unwrap(),
            now + 600_000
        );

        // Not extended
        let shorter = make_profile_dict(
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "00000000-0000AABBCCDDEEFF",
            b"CERT-A",
            now + 10_000,
        );
        assert!(compatible(
            &old,
            &shorter,
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "00000000-0000AABBCCDDEEFF",
            now
        )
        .is_err());

        // Different certificate
        let bad_cert = make_profile_dict(
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "00000000-0000AABBCCDDEEFF",
            b"CERT-B",
            now + 600_000,
        );
        assert!(compatible(
            &old,
            &bad_cert,
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "00000000-0000AABBCCDDEEFF",
            now
        )
        .is_err());

        // Different entitlements
        let mut bad_ent = good_new.clone();
        bad_ent
            .get_mut("Entitlements")
            .unwrap()
            .as_dictionary_mut()
            .unwrap()
            .insert("extra-entitlement".into(), plist::Value::Boolean(true));
        assert!(compatible(
            &old,
            &bad_ent,
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "00000000-0000AABBCCDDEEFF",
            now
        )
        .is_err());

        // Missing device
        let wrong_dev = make_profile_dict(
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "OTHER-DEVICE-UDID",
            b"CERT-A",
            now + 600_000,
        );
        assert!(compatible(
            &old,
            &wrong_dev,
            "TEAM123456",
            "com.SideStore.SideStore.TEAM123456",
            "00000000-0000AABBCCDDEEFF",
            now
        )
        .is_err());
    }

    #[test]
    fn helper_resolution_order_and_exec_check() {
        let root = unique_dir("helpers");
        let dir_a = root.join("sibling");
        let dir_b = root.join("libexec");
        let dir_c = root.join("legacy");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::create_dir_all(&dir_c).unwrap();

        let search = vec![dir_a.clone(), dir_b.clone(), dir_c.clone()];
        assert!(resolve_helper_in("profilectl", &search).is_none());

        // Non-executable file in dir_a must be skipped!
        let non_exec = dir_a.join("profilectl");
        fs::write(&non_exec, b"not exec").unwrap();
        fs::set_permissions(&non_exec, fs::Permissions::from_mode(0o644)).unwrap();

        let bin_c = dir_c.join("profilectl");
        write_script(&bin_c, "#!/bin/sh\n");
        assert_eq!(resolve_helper_in("profilectl", &search), Some(bin_c));

        let bin_b = dir_b.join("profilectl");
        write_script(&bin_b, "#!/bin/sh\n");
        assert_eq!(resolve_helper_in("profilectl", &search), Some(bin_b));

        fs::set_permissions(&non_exec, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(resolve_helper_in("profilectl", &search), Some(non_exec));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn utc_timestamp_and_report_json_schema() {
        assert_eq!(format_timestamp_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp_utc(1_764_413_575), "2025-11-29 10:52:55 UTC");

        let sys = SystemState {
            config_exists: true,
            apple_session_exists: true,
            apple_session_valid: true,
            apple_session_error: None,
            daemon_installed: true,
            daemon_enabled: true,
        };
        let now = 1_760_000_000;
        let app = app_status_from_expiry(
            "SideStore".into(),
            "com.SideStore.SideStore.TEAM123456".into(),
            now + 20 * 3600,
            now,
            48,
            false,
        );
        assert_eq!(app.status, "expiring_soon");
        assert!(app.needs_renewal);
        assert_eq!(app.hours_remaining, Some(20));

        let report = build_report("status", &sys, 48, now, vec![app], None, None);
        assert_eq!(report.status, "expiring_soon");
        let val = serde_json::to_value(&report).unwrap();
        for key in [
            "schema_version",
            "command",
            "status",
            "message",
            "config_exists",
            "apple_session_exists",
            "apple_session_valid",
            "daemon_installed",
            "daemon_enabled",
            "renew_before_hours",
            "checked_at_unix",
            "apps",
            "last_error",
        ] {
            assert!(val.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(val["schema_version"], 1);
        assert_eq!(val["apps"][0]["name"], "SideStore");
    }

    #[test]
    fn missing_config_reports_graceful_needs_setup() {
        let root = unique_dir("missing-cfg");
        let rt = RuntimeEnv {
            base: root.join("nonexistent-base"),
            daemon_plist: root.join("missing-daemon.plist"),
            helper_dirs: vec![],
        };
        let (report, code) = execute_status(&rt);
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(report.status, "needs_setup");
        assert!(!report.config_exists);
        assert!(!report.apple_session_exists);
        assert!(!report.daemon_installed);
        assert!(report.message.contains("config.plist"));

        let (ref_report, ref_code) = execute_renew(&rt, "refresh", None);
        assert_eq!(ref_code, ExitCode::FAILURE);
        assert_eq!(ref_report.status, "needs_setup");
        assert!(ref_report.last_error.is_some());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn end_to_end_status_auto_and_refresh_with_mock_helpers() {
        let root = unique_dir("e2e");
        let base = root.join("Rebox");
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();

        let helpers = root.join("bin");
        fs::create_dir_all(&helpers).unwrap();

        let state_dir = root.join("mock-mis");
        fs::create_dir_all(&state_dir).unwrap();

        let now = now_unix();
        let team = "TEAM123456";
        let device = "00000000-0000AABBCCDDEEFF";
        let b1 = "com.SideStore.SideStore.TEAM123456";
        let b2 = "org.example.FreshApp";

        // App 1 expires in 12h (needs renewal under 48h threshold)
        let p1_old = make_profile_dict(team, b1, "SideStore", device, b"CERT-1", now + 12 * 3600);
        let p1_new = make_profile_dict(team, b1, "SideStore", device, b"CERT-1", now + 168 * 3600);
        // App 2 expires in 120h (does not need auto renewal, only explicit refresh)
        let p2_old = make_profile_dict(team, b2, "FreshApp", device, b"CERT-1", now + 120 * 3600);
        let p2_new = make_profile_dict(team, b2, "FreshApp", device, b"CERT-1", now + 168 * 3600);

        write_xml_plist(&state_dir.join(format!("installed-{b1}.xml")), &p1_old, 0o600);
        write_xml_plist(&state_dir.join(format!("fresh-{b1}.xml")), &p1_new, 0o600);
        write_xml_plist(&state_dir.join(format!("installed-{b2}.xml")), &p2_old, 0o600);
        write_xml_plist(&state_dir.join(format!("fresh-{b2}.xml")), &p2_new, 0o600);

        // Mock profilectl: stores raw XML directly as the "profile" blob and updates installed file on install
        let profilectl_script = format!(
            r#"#!/bin/sh
set -eu
STATE="{}"
cmd="$1"
case "$cmd" in
  export)
    bundle="$2"
    out="$3"
    umask 077
    cp "$STATE/installed-$bundle.xml" "$out"
    ;;
  payload)
    cat "$2"
    ;;
  install)
    src="$2"
    if grep -q "{b1}" "$src"; then
      cp "$src" "$STATE/installed-{b1}.xml"
    elif grep -q "{b2}" "$src"; then
      cp "$src" "$STATE/installed-{b2}.xml"
    else
      exit 1
    fi
    ;;
  *)
    exit 1
    ;;
esac
"#,
            state_dir.display()
        );
        write_script(&helpers.join("profilectl"), &profilectl_script);

        // Mock rebox-apple: copies fresh profile with 0600 permissions
        let apple_script = format!(
            r#"#!/bin/sh
set -eu
STATE="{}"
if [ "$1" != "fetch" ]; then exit 1; fi
bundle="$4"
out="$5"
umask 077
cp "$STATE/fresh-$bundle.xml" "$out"
chmod 600 "$out"
"#,
            state_dir.display()
        );
        write_script(&helpers.join("rebox-apple"), &apple_script);

        // Write config.plist (multi-app)
        let mut cfg = plist::Dictionary::new();
        cfg.insert("team".into(), plist::Value::String(team.into()));
        cfg.insert("device".into(), plist::Value::String(device.into()));
        cfg.insert("renew_before_hours".into(), plist::Value::Integer(48.into()));
        cfg.insert(
            "bundles".into(),
            plist::Value::Array(vec![
                plist::Value::String(b1.into()),
                plist::Value::String(b2.into()),
            ]),
        );
        write_xml_plist(&base.join("config.plist"), &cfg, 0o600);
        write_session(&base, "secret@example.com", "SECRET_SPD_TOKEN_DO_NOT_LOG");

        let daemon_plist = root.join("com.bromhobit.rebox.plist");
        let mut daemon = plist::Dictionary::new();
        daemon.insert("Disabled".into(), plist::Value::Boolean(false));
        write_xml_plist(&daemon_plist, &daemon, 0o644);

        let rt = RuntimeEnv {
            base: base.clone(),
            daemon_plist,
            helper_dirs: vec![helpers],
        };

        // 1. status shows App 1 expiring_soon and App 2 ok
        let (st_report, st_code) = execute_status(&rt);
        assert_eq!(st_code, ExitCode::SUCCESS);
        assert_eq!(st_report.status, "expiring_soon");
        assert!(st_report.apple_session_valid);
        assert!(st_report.daemon_installed);
        assert!(st_report.daemon_enabled);
        assert_eq!(st_report.apps.len(), 2);
        assert_eq!(st_report.apps[0].name, "SideStore");
        assert_eq!(st_report.apps[0].status, "expiring_soon");
        assert_eq!(st_report.apps[1].name, "FreshApp");
        assert_eq!(st_report.apps[1].status, "ok");

        // 2. auto renews ONLY App 1 (within 48h) and skips App 2
        let (auto_report, auto_code) = execute_renew(&rt, "auto", None);
        assert_eq!(auto_code, ExitCode::SUCCESS, "auto report: {:?}", auto_report);
        assert_eq!(auto_report.status, "ok");
        assert!(auto_report.apps[0].renewed);
        assert!(!auto_report.apps[1].renewed);

        // Verify neither JSON nor human output ever leaks session secrets
        let json_out = serde_json::to_string(&auto_report).unwrap();
        let human_out = format_report_human(&auto_report, true);
        for out in [&json_out, &human_out] {
            assert!(!out.contains("SECRET_SPD_TOKEN_DO_NOT_LOG"));
            assert!(!out.contains("secret@example.com"));
        }

        // 3. targeted refresh --bundle renews App 2
        let (ref_report, ref_code) = execute_renew(&rt, "refresh", Some(b2));
        assert_eq!(ref_code, ExitCode::SUCCESS);
        assert_eq!(ref_report.apps.len(), 1);
        assert_eq!(ref_report.apps[0].bundle_id, b2);
        assert!(ref_report.apps[0].renewed);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cli_parser_accepts_json_before_or_after_command() {
        assert_eq!(
            parse_cli(&["status".into(), "--json".into()]).unwrap(),
            ParsedCli::Status { json: true }
        );
        assert_eq!(
            parse_cli(&["--json".into(), "status".into()]).unwrap(),
            ParsedCli::Status { json: true }
        );
        assert_eq!(
            parse_cli(&[
                "refresh".into(),
                "--json".into(),
                "--bundle".into(),
                "com.SideStore.SideStore.TEAM123456".into()
            ])
            .unwrap(),
            ParsedCli::Refresh {
                json: true,
                bundle: Some("com.SideStore.SideStore.TEAM123456".into())
            }
        );
        assert!(parse_cli(&["unknown".into()]).is_err());
    }

    #[test]
    fn lock_contention_blocks_renew_but_allows_status() {
        let root = unique_dir("lock");
        let base = root.join("Rebox");
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();

        let mut cfg = plist::Dictionary::new();
        cfg.insert("team".into(), plist::Value::String("TEAM123456".into()));
        cfg.insert("device".into(), plist::Value::String("UDID-1".into()));
        cfg.insert(
            "bundle".into(),
            plist::Value::String("com.SideStore.SideStore.TEAM123456".into()),
        );
        write_xml_plist(&base.join("config.plist"), &cfg, 0o600);

        let held_lock = acquire_lock(&base).unwrap();
        let rt = RuntimeEnv {
            base: base.clone(),
            daemon_plist: root.join("missing.plist"),
            helper_dirs: vec![],
        };

        let (renew_report, renew_code) = execute_renew(&rt, "refresh", None);
        assert_eq!(renew_code, ExitCode::FAILURE);
        assert_eq!(renew_report.status, "error");
        assert!(
            renew_report
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("уже выполняется")
        );

        // status does not acquire the exclusive lock (fails only because profilectl isn't in helper_dirs here)
        let (st_report, _) = execute_status(&rt);
        assert!(
            !st_report
                .last_error
                .as_deref()
                .unwrap_or("")
                .contains("уже выполняется")
        );

        drop(held_lock);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn build_config_from_discovered_handles_wildcards_and_devices() {
        let profiles = vec![
            DiscoveredProfile {
                bundle_id: "*".into(),
                application_identifier: "TEAM123456.*".into(),
                app_name: "Wildcard".into(),
                profile_name: "Wildcard".into(),
                team_id: "TEAM123456".into(),
                team_name: "Team".into(),
                uuid: "U0".into(),
                expires_at_unix: 2000,
                creation_date_unix: None,
                is_wildcard: true,
                provisioned_devices: vec!["DEV-1".into()],
            },
            DiscoveredProfile {
                bundle_id: "com.SideStore.SideStore.TEAM123456".into(),
                application_identifier: "TEAM123456.com.SideStore.SideStore.TEAM123456".into(),
                app_name: "SideStore".into(),
                profile_name: "SideStore".into(),
                team_id: "TEAM123456".into(),
                team_name: "Team".into(),
                uuid: "U1".into(),
                expires_at_unix: 3000,
                creation_date_unix: None,
                is_wildcard: false,
                provisioned_devices: vec!["DEV-1".into()],
            },
        ];
        let dict = build_config_from_discovered(&profiles, &[], None, None, 48).unwrap();
        let mut bytes = Vec::new();
        plist::to_writer_xml(&mut bytes, &plist::Value::Dictionary(dict)).unwrap();
        let cfg = parse_config_bytes(&bytes).unwrap();
        assert_eq!(cfg.team, "TEAM123456");
        assert_eq!(cfg.device, "DEV-1");
        assert_eq!(cfg.apps.len(), 1);
        assert_eq!(cfg.apps[0].name.as_deref(), Some("SideStore"));
    }

    #[test]
    fn migrates_legacy_dopamine_renew_data_safely() {
        use std::os::unix::fs::symlink;

        let root = unique_dir("migrate");
        let legacy = root.join("DopamineRenew");
        let target = root.join("Rebox");
        fs::DirBuilder::new().mode(0o700).create(&legacy).unwrap();
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o700)).unwrap();

        fs::write(legacy.join("config.plist"), b"<plist>legacy-cfg</plist>").unwrap();
        fs::write(legacy.join("apple-session.plist"), b"legacy-session").unwrap();
        fs::write(legacy.join("anisette_state"), b"legacy-anisette").unwrap();
        fs::write(legacy.join("daemon.log"), b"ignored-log").unwrap();
        let target_secret = root.join("outside.txt");
        fs::write(&target_secret, b"do-not-follow").unwrap();
        symlink(&target_secret, legacy.join(".sidebox-anisette-url")).unwrap();

        // Pre-existing file in target must never be overwritten.
        fs::DirBuilder::new().mode(0o700).create(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(target.join("config.plist"), b"<plist>new-cfg</plist>").unwrap();
        fs::set_permissions(target.join("config.plist"), fs::Permissions::from_mode(0o600)).unwrap();

        let copied = migrate_legacy_data(&legacy, &target).unwrap();
        assert_eq!(copied, 2);
        assert_eq!(fs::read(target.join("config.plist")).unwrap(), b"<plist>new-cfg</plist>");
        assert_eq!(fs::read(target.join("apple-session.plist")).unwrap(), b"legacy-session");
        assert_eq!(fs::read(target.join("anisette_state")).unwrap(), b"legacy-anisette");
        assert_eq!(
            fs::metadata(target.join("apple-session.plist"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!target.join("daemon.log").exists());
        assert!(!target.join(".sidebox-anisette-url").exists());
        // Source files remain untouched (non-destructive).
        assert!(legacy.join("apple-session.plist").exists());

        // Symlinked legacy dir must be ignored completely.
        let sym_root = unique_dir("migrate-sym");
        let sym_legacy = sym_root.join("SymLegacy");
        symlink(&legacy, &sym_legacy).unwrap();
        let sym_target = sym_root.join("SymTarget");
        assert_eq!(migrate_legacy_data(&sym_legacy, &sym_target).unwrap(), 0);
        assert!(!sym_target.exists());

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&sym_root);
    }
}
