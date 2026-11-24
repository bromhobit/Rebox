mod der;
use serde::Serialize;
use std::{
    env, fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_PROFILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BUNDLE_ID_LEN: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ProfileSummary {
    bundle_id: String,
    application_identifier: String,
    app_name: String,
    profile_name: String,
    team_id: String,
    team_name: String,
    uuid: String,
    expires_at_unix: u64,
    creation_date_unix: Option<u64>,
    is_wildcard: bool,
    provisioned_devices: Vec<String>,
}

fn validate_bundle_id(bundle: &str) -> Result<(), String> {
    if bundle.is_empty()
        || bundle.len() > MAX_BUNDLE_ID_LEN
        || !bundle.contains('.')
        || bundle
            .split('.')
            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    {
        return Err("invalid bundle identifier".into());
    }
    Ok(())
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn matches_bundle_id(app_id: &str, bundle: &str) -> bool {
    if app_id.contains('*') {
        return false;
    }
    app_id
        .split_once('.')
        .is_some_and(|(prefix, suffix)| !prefix.is_empty() && suffix == bundle)
}

fn validate_profile_size(len: usize) -> Result<(), String> {
    if len == 0 || len > MAX_PROFILE_BYTES {
        return Err("invalid profile size".into());
    }
    Ok(())
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn write_private_file(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    validate_profile_size(bytes.len())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(|_| "cannot create output file")?;
    if file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .is_err()
    {
        let _ = fs::remove_file(dest);
        return Err("cannot write profile".into());
    }
    Ok(())
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn plist_date_to_unix(dict: &plist::Dictionary, key: &str) -> Option<u64> {
    let date = dict.get(key).and_then(plist::Value::as_date)?;
    SystemTime::from(date)
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn parse_profile_metadata(payload_xml: &[u8]) -> Result<ProfileSummary, String> {
    validate_profile_size(payload_xml.len())?;
    let dict: plist::Dictionary =
        plist::from_bytes(payload_xml).map_err(|_| "invalid decoded profile plist")?;
    let application_identifier = dict
        .get("Entitlements")
        .and_then(plist::Value::as_dictionary)
        .and_then(|e| e.get("application-identifier"))
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_string();
    let (prefix_team, bundle_id) = application_identifier
        .split_once('.')
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .unwrap_or_else(|| (String::new(), application_identifier.clone()));
    let team_id = dict
        .get("TeamIdentifier")
        .and_then(plist::Value::as_array)
        .and_then(|arr| arr.iter().find_map(plist::Value::as_string))
        .map(ToOwned::to_owned)
        .unwrap_or(prefix_team);
    let team_name = dict
        .get("TeamName")
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_string();
    let app_name = dict
        .get("AppIDName")
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_string();
    let profile_name = dict
        .get("Name")
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_string();
    let uuid = dict
        .get("UUID")
        .and_then(plist::Value::as_string)
        .unwrap_or_default()
        .to_string();
    let expires_at_unix =
        plist_date_to_unix(&dict, "ExpirationDate").ok_or("profile expiration is missing")?;
    let creation_date_unix = plist_date_to_unix(&dict, "CreationDate");
    let provisioned_devices = dict
        .get("ProvisionedDevices")
        .and_then(plist::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(plist::Value::as_string)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let is_wildcard = application_identifier.contains('*');
    Ok(ProfileSummary {
        bundle_id,
        application_identifier,
        app_name,
        profile_name,
        team_id,
        team_name,
        uuid,
        expires_at_unix,
        creation_date_unix,
        is_wildcard,
        provisioned_devices,
    })
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn sort_profiles(profiles: &mut [ProfileSummary]) {
    profiles.sort_by(|a, b| {
        b.expires_at_unix
            .cmp(&a.expires_at_unix)
            .then_with(|| {
                b.creation_date_unix
                    .unwrap_or(0)
                    .cmp(&a.creation_date_unix.unwrap_or(0))
            })
            .then_with(|| a.bundle_id.cmp(&b.bundle_id))
            .then_with(|| a.uuid.cmp(&b.uuid))
    });
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn select_best_profile_index(candidates: &[(&ProfileSummary, usize)], bundle: &str) -> Option<usize> {
    candidates
        .iter()
        .filter(|(summary, _)| matches_bundle_id(&summary.application_identifier, bundle))
        .max_by_key(|(summary, _)| {
            (
                summary.expires_at_unix,
                summary.creation_date_unix.unwrap_or(0),
            )
        })
        .map(|(_, idx)| *idx)
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn format_list_json(profiles: &[ProfileSummary]) -> Result<String, String> {
    serde_json::to_string_pretty(profiles).map_err(|_| "cannot serialize profile list".into())
}

#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
fn format_list_human(profiles: &[ProfileSummary]) -> String {
    if profiles.is_empty() {
        return "No installed provisioning profiles found.".to_string();
    }
    let mut lines = Vec::with_capacity(profiles.len());
    for p in profiles {
        let label = if !p.app_name.is_empty() {
            &p.app_name
        } else if !p.profile_name.is_empty() {
            &p.profile_name
        } else {
            &p.bundle_id
        };
        lines.push(format!(
            "{}\t{}\tteam={}\tuuid={}\texpires={}",
            p.bundle_id, label, p.team_id, p.uuid, p.expires_at_unix
        ));
    }
    lines.join("\n")
}

#[cfg(target_os = "ios")]
mod ios {
    use super::*;
    use std::{
        ffi::{CStr, c_char, c_int, c_void},
        sync::OnceLock,
    };

    type CfRef = *const c_void;
    type CreateFn = unsafe extern "C" fn(CfRef, CfRef) -> CfRef;
    type InstallFn = unsafe extern "C" fn(CfRef) -> i32;
    type CopyPayloadFn = unsafe extern "C" fn(CfRef) -> CfRef;

    const SQLITE_OK: c_int = 0;
    const SQLITE_ROW: c_int = 100;
    const SQLITE_DONE: c_int = 101;
    const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
    const SQLITE_OPEN_URI: c_int = 0x0000_0040;
    const MIS_DB_PATH: &str = "/var/MobileDevice/ProvisioningProfiles/mis.db";
    const MIS_DIR_PATH: &str = "/var/MobileDevice/ProvisioningProfiles";

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFDataCreate(allocator: CfRef, bytes: *const u8, len: isize) -> CfRef;
        fn CFRelease(object: CfRef);
        fn CFDataGetLength(data: CfRef) -> isize;
        fn CFDataGetBytePtr(data: CfRef) -> *const u8;
        fn CFPropertyListCreateData(
            allocator: CfRef,
            property_list: CfRef,
            format: i32,
            options: usize,
            error: *mut CfRef,
        ) -> CfRef;
    }

    #[link(name = "sqlite3")]
    unsafe extern "C" {
        fn sqlite3_open_v2(
            filename: *const c_char,
            db: *mut *mut c_void,
            flags: c_int,
            vfs: *const c_char,
        ) -> c_int;
        fn sqlite3_busy_timeout(db: *mut c_void, ms: c_int) -> c_int;
        fn sqlite3_prepare_v2(
            db: *mut c_void,
            sql: *const c_char,
            size: c_int,
            stmt: *mut *mut c_void,
            tail: *mut *const c_char,
        ) -> c_int;
        fn sqlite3_step(stmt: *mut c_void) -> c_int;
        fn sqlite3_column_blob(stmt: *mut c_void, n: c_int) -> *const u8;
        fn sqlite3_column_bytes(stmt: *mut c_void, n: c_int) -> c_int;
        fn sqlite3_finalize(stmt: *mut c_void) -> c_int;
        fn sqlite3_close(db: *mut c_void) -> c_int;
        fn sqlite3_errmsg(db: *mut c_void) -> *const c_char;
    }

    struct LibMis {
        create: CreateFn,
        install: InstallFn,
        copy_payload: CopyPayloadFn,
    }

    static LIBMIS: OnceLock<Result<LibMis, String>> = OnceLock::new();

    fn libmis() -> Result<&'static LibMis, String> {
        LIBMIS
            .get_or_init(|| unsafe {
                let lib = dlopen(c"/usr/lib/libmis.dylib".as_ptr(), 2);
                if lib.is_null() {
                    return Err("libmis unavailable".into());
                }
                let create = dlsym(lib, c"MISProfileCreateWithData".as_ptr());
                let install = dlsym(lib, c"MISInstallProvisioningProfile".as_ptr());
                let copy_payload = dlsym(lib, c"MISProfileCopyPayload".as_ptr());
                if create.is_null() || install.is_null() || copy_payload.is_null() {
                    return Err("libmis functions unavailable".into());
                }
                Ok(LibMis {
                    create: std::mem::transmute(create),
                    install: std::mem::transmute(install),
                    copy_payload: std::mem::transmute(copy_payload),
                })
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn install(path: &Path) -> Result<(), String> {
        let bytes = fs::read(path).map_err(|_| "cannot read profile")?;
        validate_profile_size(bytes.len())?;
        let mis = libmis()?;
        unsafe {
            let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
            if data.is_null() {
                return Err("cannot allocate profile data".into());
            }
            let profile = (mis.create)(std::ptr::null(), data);
            CFRelease(data);
            if profile.is_null() {
                return Err("system rejected profile data".into());
            }
            let status = (mis.install)(profile);
            CFRelease(profile);
            if status != 0 {
                return Err(format!("profile service returned status {status:#x}"));
            }
        }
        Ok(())
    }

    pub fn payload(bytes: &[u8]) -> Result<Vec<u8>, String> {
        validate_profile_size(bytes.len())?;
        let mis = libmis()?;
        unsafe {
            let data = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
            if data.is_null() {
                return Err("cannot allocate profile data".into());
            }
            let profile = (mis.create)(std::ptr::null(), data);
            CFRelease(data);
            if profile.is_null() {
                return Err("system rejected profile data".into());
            }
            let result = (mis.copy_payload)(profile);
            CFRelease(profile);
            if result.is_null() {
                return Err("cannot decode profile".into());
            }
            let xml =
                CFPropertyListCreateData(std::ptr::null(), result, 100, 0, std::ptr::null_mut());
            CFRelease(result);
            if xml.is_null() {
                return Err("cannot serialize profile".into());
            }
            let count = CFDataGetLength(xml);
            if !(1..=MAX_PROFILE_BYTES as isize).contains(&count) {
                CFRelease(xml);
                return Err("invalid decoded profile size".into());
            }
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(xml), count as usize).to_vec();
            CFRelease(xml);
            der::normalize_profile(&bytes)
        }
    }

    unsafe fn sqlite_error(db: *mut c_void) -> String {
        if db.is_null() {
            return "unknown sqlite error".to_string();
        }
        let msg = unsafe { sqlite3_errmsg(db) };
        if msg.is_null() {
            "unknown sqlite error".to_string()
        } else {
            unsafe { CStr::from_ptr(msg) }
                .to_string_lossy()
                .into_owned()
        }
    }

    fn query_mis_db(target: &CStr, flags: c_int) -> Result<Vec<Vec<u8>>, String> {
        let mut db = std::ptr::null_mut();
        let mut stmt = std::ptr::null_mut();
        let mut blobs = Vec::new();
        unsafe {
            if sqlite3_open_v2(target.as_ptr(), &mut db, flags, std::ptr::null()) != SQLITE_OK {
                let err = sqlite_error(db);
                if !db.is_null() {
                    sqlite3_close(db);
                }
                return Err(err);
            }
            sqlite3_busy_timeout(db, 3000);
            if sqlite3_prepare_v2(
                db,
                c"SELECT cms_blob FROM profiles ORDER BY expires DESC".as_ptr(),
                -1,
                &mut stmt,
                std::ptr::null_mut(),
            ) != SQLITE_OK
            {
                let err = sqlite_error(db);
                sqlite3_close(db);
                return Err(err);
            }
            let mut query_err = None;
            loop {
                let step = sqlite3_step(stmt);
                if step == SQLITE_DONE {
                    break;
                }
                if step != SQLITE_ROW {
                    query_err = Some(sqlite_error(db));
                    break;
                }
                let len = sqlite3_column_bytes(stmt, 0);
                let ptr = sqlite3_column_blob(stmt, 0);
                if ptr.is_null() || !(1..=MAX_PROFILE_BYTES as c_int).contains(&len) {
                    continue;
                }
                blobs.push(std::slice::from_raw_parts(ptr, len as usize).to_vec());
            }
            sqlite3_finalize(stmt);
            sqlite3_close(db);
            if let Some(err) = query_err {
                return Err(err);
            }
        }
        Ok(blobs)
    }

    fn read_fallback_profile_files() -> Result<Vec<Vec<u8>>, String> {
        let entries = fs::read_dir(MIS_DIR_PATH)
            .map_err(|e| format!("cannot scan {MIS_DIR_PATH}: {e}"))?;
        let mut blobs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("mobileprovision") {
                continue;
            }
            if let Ok(meta) = fs::symlink_metadata(&path) {
                if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_PROFILE_BYTES as u64 {
                    continue;
                }
            }
            if let Ok(bytes) = fs::read(&path) {
                if validate_profile_size(bytes.len()).is_ok() {
                    blobs.push(bytes);
                }
            }
        }
        Ok(blobs)
    }

    fn read_installed_blobs() -> Result<Vec<Vec<u8>>, String> {
        let primary_err = match query_mis_db(
            c"/var/MobileDevice/ProvisioningProfiles/mis.db",
            SQLITE_OPEN_READONLY,
        ) {
            Ok(blobs) => return Ok(blobs),
            Err(err) => err,
        };
        if let Ok(blobs) = query_mis_db(
            c"file:/var/MobileDevice/ProvisioningProfiles/mis.db?mode=ro&immutable=1",
            SQLITE_OPEN_READONLY | SQLITE_OPEN_URI,
        ) {
            return Ok(blobs);
        }
        if let Ok(blobs) = read_fallback_profile_files() {
            if !blobs.is_empty() {
                return Ok(blobs);
            }
        }
        Err(format!(
            "cannot read installed profiles from {MIS_DB_PATH}: {primary_err}"
        ))
    }

    pub fn export(bundle: &str, dest: &Path) -> Result<(), String> {
        validate_bundle_id(bundle)?;
        let blobs = read_installed_blobs()?;
        let mut decoded = Vec::new();
        for (idx, bytes) in blobs.iter().enumerate() {
            let Ok(xml) = payload(bytes) else {
                continue;
            };
            let Ok(summary) = parse_profile_metadata(&xml) else {
                continue;
            };
            decoded.push((summary, idx));
        }
        let refs: Vec<(&ProfileSummary, usize)> =
            decoded.iter().map(|(s, i)| (s, *i)).collect();
        let best = select_best_profile_index(&refs, bundle).ok_or("profile not found")?;
        write_private_file(dest, &blobs[best])
    }

    pub fn list(human: bool) -> Result<String, String> {
        let blobs = read_installed_blobs()?;
        let mut profiles = Vec::new();
        for bytes in &blobs {
            let Ok(xml) = payload(bytes) else {
                continue;
            };
            if let Ok(summary) = parse_profile_metadata(&xml) {
                profiles.push(summary);
            }
        }
        sort_profiles(&mut profiles);
        if human {
            Ok(format_list_human(&profiles))
        } else {
            format_list_json(&profiles)
        }
    }
}

#[cfg(not(target_os = "ios"))]
mod ios {
    use super::*;

    pub fn install(path: &Path) -> Result<(), String> {
        let bytes = fs::read(path).map_err(|_| "cannot read profile")?;
        validate_profile_size(bytes.len())?;
        Err("profilectl install requires iOS runtime (libmis)".into())
    }

    pub fn payload(bytes: &[u8]) -> Result<Vec<u8>, String> {
        validate_profile_size(bytes.len())?;
        Err("profilectl payload requires iOS runtime (libmis)".into())
    }

    pub fn export(bundle: &str, _dest: &Path) -> Result<(), String> {
        validate_bundle_id(bundle)?;
        Err("profilectl export requires iOS runtime (mis.db)".into())
    }

    pub fn list(_human: bool) -> Result<String, String> {
        Err("profilectl list requires iOS runtime (mis.db)".into())
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [cmd, file] if cmd == "payload" => match fs::read(file)
            .map_err(|_| "cannot read profile".to_string())
            .and_then(|data| ios::payload(&data))
        {
            Ok(xml) => match std::io::stdout().write_all(&xml) {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::FAILURE,
            },
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        [cmd, bundle, output] if cmd == "export" => match bundle
            .to_str()
            .ok_or("invalid bundle".to_string())
            .and_then(|bundle| ios::export(bundle, Path::new(output)))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        [cmd, file] if cmd == "install" => match ios::install(Path::new(file)) {
            Ok(()) => {
                println!("Profile service reported success");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        [cmd] if cmd == "list" => match ios::list(false) {
            Ok(out) => {
                println!("{out}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        },
        [cmd, flag] if cmd == "list" && (flag == "--json" || flag == "--human") => {
            match ios::list(flag == "--human") {
                Ok(out) => {
                    println!("{out}");
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!(
                "Usage: profilectl install PROFILE | payload PROFILE | export BUNDLE OUTPUT | list [--json|--human]"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn sample_plist_xml(
        app_id: &str,
        app_name: &str,
        team_id: &str,
        uuid: &str,
        expires: &str,
        created: &str,
    ) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AppIDName</key><string>{app_name}</string>
    <key>Name</key><string>iOS Team Provisioning Profile: {app_id}</string>
    <key>TeamIdentifier</key><array><string>{team_id}</string></array>
    <key>TeamName</key><string>Test Team</string>
    <key>UUID</key><string>{uuid}</string>
    <key>CreationDate</key><date>{created}</date>
    <key>ExpirationDate</key><date>{expires}</date>
    <key>ProvisionedDevices</key><array><string>00000000-0000AABBCCDDEEFF</string></array>
    <key>Entitlements</key>
    <dict>
        <key>application-identifier</key><string>{app_id}</string>
    </dict>
</dict>
</plist>"#
        )
        .into_bytes()
    }

    #[test]
    fn bundle_id_validation_rules() {
        assert!(validate_bundle_id("com.SideStore.SideStore.TEAM123456").is_ok());
        assert!(validate_bundle_id("org.my-app.v2").is_ok());
        assert!(validate_bundle_id("").is_err());
        assert!(validate_bundle_id("nodot").is_err());
        assert!(validate_bundle_id("com..empty").is_err());
        assert!(validate_bundle_id(".leading.dot").is_err());
        assert!(validate_bundle_id("com.bad/slash").is_err());
        assert!(validate_bundle_id("com.wild.*").is_err());
        assert!(validate_bundle_id(&format!("com.{}", "a".repeat(260))).is_err());
    }

    #[test]
    fn wildcard_rejected_in_bundle_matching() {
        assert!(matches_bundle_id(
            "TEAM123456.com.SideStore.SideStore.TEAM123456",
            "com.SideStore.SideStore.TEAM123456"
        ));
        assert!(!matches_bundle_id(
            "TEAM123456.*",
            "com.SideStore.SideStore.TEAM123456"
        ));
        assert!(!matches_bundle_id(
            ".com.SideStore.SideStore",
            "com.SideStore.SideStore"
        ));
    }

    #[test]
    fn parse_and_select_newest_profile() {
        let old_xml = sample_plist_xml(
            "TEAM123456.com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "TEAM123456",
            "1111-OLD",
            "2026-11-25T10:00:00Z",
            "2026-11-18T10:00:00Z",
        );
        let new_xml = sample_plist_xml(
            "TEAM123456.com.SideStore.SideStore.TEAM123456",
            "SideStore",
            "TEAM123456",
            "2222-NEW",
            "2026-11-29T10:52:55Z",
            "2026-11-22T10:52:55Z",
        );
        let old_p = parse_profile_metadata(&old_xml).unwrap();
        let new_p = parse_profile_metadata(&new_xml).unwrap();
        assert_eq!(new_p.bundle_id, "com.SideStore.SideStore.TEAM123456");
        assert_eq!(new_p.app_name, "SideStore");
        assert_eq!(new_p.team_id, "TEAM123456");
        assert_eq!(new_p.uuid, "2222-NEW");
        assert!(!new_p.is_wildcard);
        assert_eq!(new_p.provisioned_devices, vec!["00000000-0000AABBCCDDEEFF"]);

        let candidates = vec![(&old_p, 0usize), (&new_p, 1usize)];
        assert_eq!(
            select_best_profile_index(&candidates, "com.SideStore.SideStore.TEAM123456"),
            Some(1)
        );

        let mut list = vec![old_p, new_p];
        sort_profiles(&mut list);
        assert_eq!(list[0].uuid, "2222-NEW");
        let json = format_list_json(&list).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["uuid"], "2222-NEW");

        let human = format_list_human(&list);
        assert!(human.contains("com.SideStore.SideStore.TEAM123456\tSideStore\tteam=TEAM123456\tuuid=2222-NEW"));
        assert_eq!(
            format_list_human(&[]),
            "No installed provisioning profiles found."
        );
    }

    #[test]
    fn write_private_file_enforces_0600_and_create_new() {
        let dir = env::temp_dir().join(format!("profilectl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.mobileprovision");
        write_private_file(&target, b"hello").unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(write_private_file(&target, b"overwrite").is_err());
        assert!(write_private_file(&dir.join("empty"), b"").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
