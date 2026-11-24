//! One-way, local session integration: Sidebox is the source of Apple authentication.
//! Never print, transmit or embed the contents of an Apple session in diagnostics.
use plist::Dictionary;
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
const NEW_BUNDLE_BASE: &str = "com.bromhobit.Sidebox";
const LEGACY_BUNDLE_BASE: &str = "org.codex.SideBox";
const META: &str = ".com.apple.mobile_container_manager.metadata.plist";
const SESSION: &str = "apple-session.plist";
const ANISETTE: &str = "anisette_state";
const ANISETTE_URL: &str = ".sidebox-anisette-url";
const SETTINGS: &str = "settings.json";
const DEFAULT_ANISETTE_URL: &str = "https://mac-ani.jkcoxson.com";
const MARKER: &str = ".sidebox-session-link";
const MAX_BYTES: u64 = 1024 * 1024;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn matches_bundle_family(id: &str, base: &str) -> bool {
    if id == base {
        return true;
    }
    let Some(team) = id.strip_prefix(base).and_then(|s| s.strip_prefix('.')) else {
        return false;
    };
    !team.is_empty()
        && team.len() <= 32
        && !team.contains('.')
        && team.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn regular(path: &Path, max: u64) -> Result<fs::Metadata, String> {
    let m = fs::symlink_metadata(path).map_err(|_| "недоступен файл Sidebox".to_string())?;
    if !m.is_file() || m.file_type().is_symlink() || m.len() == 0 || m.len() > max {
        return Err("некорректный файл Sidebox".into());
    }
    Ok(m)
}

fn locate(root: &Path) -> Result<Option<PathBuf>, String> {
    let mut found_new: Option<PathBuf> = None;
    let mut found_legacy: Option<PathBuf> = None;
    let mut legacy_ambiguous = false;
    for entry in fs::read_dir(root).map_err(|_| "контейнеры iOS недоступны")? {
        let entry = entry.map_err(|_| "ошибка чтения контейнеров iOS")?;
        let container = entry.path();
        let Ok(m) = fs::symlink_metadata(&container) else { continue };
        if !m.is_dir() || m.file_type().is_symlink() { continue; }
        let metadata = container.join(META);
        if regular(&metadata, 65536).is_err() { continue; }
        let Ok(d) = plist::from_file::<_, Dictionary>(&metadata) else { continue };
        let Some(id) = d.get("MCMMetadataIdentifier").and_then(plist::Value::as_string) else { continue };
        if matches_bundle_family(id, NEW_BUNDLE_BASE) {
            if found_new.is_some() {
                return Err("обнаружено несколько контейнеров Sidebox".into());
            }
            found_new = Some(container.join("Documents").join(SESSION));
        } else if matches_bundle_family(id, LEGACY_BUNDLE_BASE) {
            if found_legacy.is_some() {
                legacy_ambiguous = true;
            } else {
                found_legacy = Some(container.join("Documents").join(SESSION));
            }
        }
    }
    if let Some(new_path) = found_new {
        if fs::symlink_metadata(&new_path).is_ok() {
            return Ok(Some(new_path));
        }
        if !legacy_ambiguous
            && let Some(leg_path) = found_legacy
            && fs::symlink_metadata(&leg_path).is_ok()
        {
            return Ok(Some(leg_path));
        }
        return Ok(Some(new_path));
    }
    if legacy_ambiguous {
        return Err("обнаружено несколько контейнеров Sidebox".into());
    }
    Ok(found_legacy)
}

fn validate_private_dir(base: &Path) -> Result<(), String> {
    let m = fs::symlink_metadata(base).map_err(|_| "каталог Rebox отсутствует")?;
    if m.file_type().is_symlink() || !m.is_dir() || m.permissions().mode() & 0o077 != 0 {
        return Err("каталог Rebox должен иметь права 0700".into());
    }
    Ok(())
}

fn validate_session(path: &Path) -> Result<Vec<u8>, String> {
    let m = regular(path, MAX_BYTES)?;
    if m.permissions().mode() & 0o077 != 0 {
        return Err("файл сессии Sidebox должен иметь права 0600".into());
    }
    // The session must be owned by the account running the daemon; never
    // trust arbitrary writable files in a neighboring application container.
    if m.uid() != unsafe { libc::geteuid() } {
        return Err("неверный владелец сессии Sidebox".into());
    }
    let bytes = fs::read(path).map_err(|_| "не удалось прочитать сессию Sidebox")?;
    let (email, state): (String, Dictionary) =
        plist::from_bytes(&bytes).map_err(|_| "неверный формат сессии Sidebox")?;
    if email.trim().is_empty() || state.is_empty() {
        return Err("сессия Sidebox неполна".into());
    }
    Ok(bytes)
}

// Anisette state belongs to the same account session. Read it from the same
// vetted Sidebox Documents directory; never log its contents.
fn validate_anisette(path: &Path) -> Result<Vec<u8>, String> {
    let meta = regular(path, MAX_BYTES).map_err(|_| "отсутствует состояние Anisette Sidebox")?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o022 != 0 {
        return Err("небезопасные права состояния Anisette Sidebox".into());
    }
    fs::read(path).map_err(|_| "не удалось прочитать состояние Anisette Sidebox".into())
}

fn valid_anisette_url(url: &str) -> bool {
    if url.is_empty() || url.len() > 2048 || url.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20)
        || url.contains(['@', '#']) { return false; }
    let (secure, host) = if let Some(host) = url.strip_prefix("https://") {
        (true, host)
    } else if let Some(host) = url.strip_prefix("http://") {
        (false, host)
    } else { return false; };
    let hostname = host.split(['/', ':', '?']).next().unwrap_or_default();
    !hostname.is_empty() && (secure || matches!(hostname, "localhost" | "127.0.0.1" | "[::1]"))
}

fn read_anisette_url(documents: &Path) -> Result<String, String> {
    let path = documents.join(SETTINGS);
    let bytes = match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(DEFAULT_ANISETTE_URL.into()),
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink()
            && meta.len() > 0 && meta.len() <= 65536 => {
            fs::read(&path).map_err(|_| "не удалось прочитать настройки Sidebox")?
        }
        _ => return Err("некорректный файл настроек Sidebox".into()),
    };
    let settings: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| "не удалось прочитать URL Anisette Sidebox")?;
    let url = settings.get("anisette_url").and_then(serde_json::Value::as_str)
        .unwrap_or(DEFAULT_ANISETTE_URL).trim();
    if !valid_anisette_url(url) { return Err("недопустимый URL Anisette Sidebox".into()); }
    Ok(url.into())
}

fn sync_file(base: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let target = base.join(name);
    match fs::symlink_metadata(&target) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            return Err("небезопасный целевой файл синхронизации Sidebox".into());
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err("не удалось проверить целевой файл Sidebox".into()),
        _ => {}
    }
    if fs::read(&target).ok().as_deref() != Some(bytes) {
        write_atomic(base, name, bytes)?;
    }
    Ok(())
}

fn write_atomic(base: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|x| x.as_nanos()).unwrap_or(0);
    let tmp = base.join(format!(".{name}.tmp-{}-{nanos}-{seq}", std::process::id()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, base.join(name))?;
        Ok(())
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result.map_err(|_| "не удалось атомарно синхронизировать сессию Sidebox".into())
}

fn unlink_if_regular(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => {
            fs::remove_file(path).map_err(|_| "не удалось очистить сессию Sidebox".into())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err("небезопасный файл сессии, отказ от удаления".into()),
    }
}

/// Synchronize a session without opening an interactive login.
/// Existing standalone Rebox sessions remain untouched unless
/// they were already linked from Sidebox via the marker.
pub fn sync(base: &Path, containers_root: &Path) -> Result<&'static str, String> {
    validate_private_dir(base)?;
    let marker_path = base.join(MARKER);
    let linked = match fs::symlink_metadata(&marker_path) {
        Ok(m) if m.is_file() && !m.file_type().is_symlink() && m.len() <= 64
            && m.permissions().mode() & 0o077 == 0 =>
        {
            fs::read(&marker_path).ok().as_deref() == Some(b"sidebox-v1\n")
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        _ => return Err("некорректный маркер интеграции Sidebox".into()),
    };
    let target = base.join(SESSION);
    if !linked && fs::symlink_metadata(&target).is_ok() {
        return Ok("standalone");
    }
    let source = locate(containers_root)?;
    if let Some(path) = source {
        let session_meta = fs::symlink_metadata(&path);
        if session_meta.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
            // No account is logged in to Sidebox anymore.
            if linked {
                for name in [SESSION, ANISETTE, ANISETTE_URL, MARKER] {
                    unlink_if_regular(&base.join(name))?;
                }
                return Ok("sidebox-logged-out");
            }
            return Ok("missing");
        }
        // Build and validate the whole session state before changing anything.
        // No live device state is inspected or copied by the build process.
        let documents = path.parent().ok_or("некорректный контейнер Sidebox")?;
        let documents_meta = fs::symlink_metadata(documents)
            .map_err(|_| "каталог Sidebox недоступен")?;
        if documents_meta.file_type().is_symlink() || !documents_meta.is_dir() {
            return Err("небезопасный каталог Sidebox".into());
        }
        let session = validate_session(&path)?;
        let anisette = validate_anisette(&documents.join(ANISETTE))?;
        let url = read_anisette_url(documents)?;
        // Write Anisette and URL first, session last: a newly imported
        // session is never visible without its matching Anisette material.
        sync_file(base, ANISETTE, &anisette)?;
        sync_file(base, ANISETTE_URL, url.as_bytes())?;
        sync_file(base, SESSION, &session)?;
        if !linked { sync_file(base, MARKER, b"sidebox-v1\n")?; }
        return Ok("sidebox");
    }
    if linked {
        for name in [SESSION, ANISETTE, ANISETTE_URL, MARKER] {
            unlink_if_regular(&base.join(name))?;
        }
        return Ok("sidebox-logged-out");
    }
    Ok("missing")
}

pub fn sync_production(base: &Path) -> Result<&'static str, String> {
    sync(base, Path::new("/var/mobile/Containers/Data/Application"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn add_container(root: &Path, dir_name: &str, bundle_id: &str) -> PathBuf {
        let app = root.join(dir_name);
        fs::create_dir_all(app.join("Documents")).unwrap();
        let mut d = Dictionary::new();
        d.insert(
            "MCMMetadataIdentifier".into(),
            plist::Value::String(bundle_id.into()),
        );
        plist::to_file_xml(app.join(META), &d).unwrap();
        app
    }

    fn prep_with_bundle(bundle_id: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "sidebox-bridge-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let ctrl = base.join("renew");
        let root = base.join("containers");
        fs::create_dir_all(&ctrl).unwrap();
        fs::set_permissions(&ctrl, fs::Permissions::from_mode(0o700)).unwrap();
        let app = add_container(&root, "A1B2C3", bundle_id);
        (ctrl, root, app)
    }

    fn prep() -> (PathBuf, PathBuf, PathBuf) {
        prep_with_bundle("com.bromhobit.Sidebox.TEAM123456")
    }

    fn session(path: &Path, token: &str) {
        let mut spd = Dictionary::new();
        spd.insert("GSIdmsToken".into(), plist::Value::String(token.into()));
        plist::to_file_binary(path, &("user@example.test", spd)).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn copies_updates_and_revokes_on_logout() {
        let (base, root, app) = prep();
        let source = app.join("Documents").join(SESSION);
        session(&source, "TOKEN-A");
        fs::write(app.join("Documents").join(ANISETTE), b"ANISETTE-A").unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox");
        assert_eq!(
            fs::read(base.join(SESSION)).unwrap(),
            fs::read(&source).unwrap()
        );
        session(&source, "TOKEN-B");
        fs::write(app.join("Documents").join(ANISETTE), b"ANISETTE-B").unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox");
        assert_eq!(
            fs::read(base.join(SESSION)).unwrap(),
            fs::read(&source).unwrap()
        );
        assert_eq!(fs::read(base.join(ANISETTE)).unwrap(), b"ANISETTE-B");
        fs::remove_file(source).unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox-logged-out");
        assert!(!base.join(SESSION).exists());
        assert!(!base.join(ANISETTE).exists());
        assert!(!base.join(ANISETTE_URL).exists());
        let _ = fs::remove_dir_all(base.parent().unwrap().join("nonexistent"));
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn supports_legacy_suffixed_and_exact_bundles() {
        for legacy in ["org.codex.SideBox.TEAM123456", "org.codex.SideBox"] {
            let (base, root, app) = prep_with_bundle(legacy);
            let docs = app.join("Documents");
            session(&docs.join(SESSION), "LEGACY-TOK");
            fs::write(docs.join(ANISETTE), b"LEGACY-ANI").unwrap();
            assert_eq!(sync(&base, &root).unwrap(), "sidebox");
            let (_, dict): (String, Dictionary) = plist::from_file(base.join(SESSION)).unwrap();
            assert_eq!(dict["GSIdmsToken"].as_string(), Some("LEGACY-TOK"));
            fs::remove_dir_all(root.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn coexistence_prefers_new_sidebox_with_legacy_fallback() {
        let (base, root, new_app) = prep_with_bundle("com.bromhobit.Sidebox.TEAM123456");
        let legacy_app = add_container(&root, "OLD123", "org.codex.SideBox.TEAM123456");

        // Legacy active, new Sidebox installed without session yet -> falls back to legacy
        let leg_docs = legacy_app.join("Documents");
        session(&leg_docs.join(SESSION), "TOK-LEGACY");
        fs::write(leg_docs.join(ANISETTE), b"ANI-LEGACY").unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox");
        let (_, d1): (String, Dictionary) = plist::from_file(base.join(SESSION)).unwrap();
        assert_eq!(d1["GSIdmsToken"].as_string(), Some("TOK-LEGACY"));

        // New Sidebox gets logged in -> switches cleanly to new container
        let new_docs = new_app.join("Documents");
        session(&new_docs.join(SESSION), "TOK-NEW");
        fs::write(new_docs.join(ANISETTE), b"ANI-NEW").unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox");
        let (_, d2): (String, Dictionary) = plist::from_file(base.join(SESSION)).unwrap();
        assert_eq!(d2["GSIdmsToken"].as_string(), Some("TOK-NEW"));
        assert_eq!(fs::read(base.join(ANISETTE)).unwrap(), b"ANI-NEW");

        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn ignores_extension_containers_and_rejects_duplicate_new_containers() {
        let (base, root, app) = prep_with_bundle("com.bromhobit.Sidebox.TEAM123456");
        let _ext = add_container(&root, "EXT999", "com.bromhobit.Sidebox.TEAM123456.Widget");
        let docs = app.join("Documents");
        session(&docs.join(SESSION), "MAIN-TOK");
        fs::write(docs.join(ANISETTE), b"MAIN-ANI").unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox");

        let _dup = add_container(&root, "DUP222", "com.bromhobit.Sidebox.OTHERTEAM1");
        assert!(sync(&base, &root).is_err());

        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn preserves_unlinked_standalone_session() {
        let (base, root, app) = prep();
        session(&base.join(SESSION), "NATIVE");
        session(&app.join("Documents").join(SESSION), "SIDEBOX");
        assert_eq!(sync(&base, &root).unwrap(), "standalone");
        let (mail, dict): (String, Dictionary) = plist::from_file(base.join(SESSION)).unwrap();
        assert_eq!(mail, "user@example.test");
        assert_eq!(dict["GSIdmsToken"].as_string(), Some("NATIVE"));
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn refuses_missing_anisette_without_importing_session() {
        let (base, root, app) = prep();
        session(&app.join("Documents").join(SESSION), "SYNTHETIC");
        assert!(sync(&base, &root).is_err());
        assert!(!base.join(SESSION).exists());
        assert!(!base.join(MARKER).exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn imports_custom_https_server_and_refuses_insecure_http() {
        let (base, root, app) = prep();
        let docs = app.join("Documents");
        session(&docs.join(SESSION), "SYNTHETIC");
        fs::write(docs.join(ANISETTE), b"STATE").unwrap();
        fs::write(
            docs.join(SETTINGS),
            br#"{"anisette_url":"https://example.net/api"}"#,
        )
        .unwrap();
        assert_eq!(sync(&base, &root).unwrap(), "sidebox");
        assert_eq!(
            fs::read_to_string(base.join(ANISETTE_URL)).unwrap(),
            "https://example.net/api"
        );
        assert_eq!(
            fs::metadata(base.join(ANISETTE)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::write(
            docs.join(SETTINGS),
            br#"{"anisette_url":"http://example.net/api"}"#,
        )
        .unwrap();
        assert!(sync(&base, &root).is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_symlink_anisette() {
        let (base, root, app) = prep();
        let docs = app.join("Documents");
        session(&docs.join(SESSION), "SYNTHETIC");
        let real = docs.join("real");
        fs::write(&real, b"STATE").unwrap();
        std::os::unix::fs::symlink(&real, docs.join(ANISETTE)).unwrap();
        assert!(sync(&base, &root).is_err());
        assert!(!base.join(SESSION).exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_world_readable_sessions() {
        let (base, root, app) = prep();
        let src = app.join("Documents").join(SESSION);
        session(&src, "FAKE");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(sync(&base, &root).is_err());
        assert!(!base.join(SESSION).exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
}
