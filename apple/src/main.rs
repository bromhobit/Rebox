use isideload::{
    anisette::remote_v3::RemoteV3AnisetteProvider,
    auth::apple_account::{AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse},
    dev::developer_session::{AppIdsApi, DeveloperDeviceType, DeveloperSession, TeamsApi},
    util::fs_storage::FsStorage,
};
use serde::Serialize;
use std::{
    env, fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::ExitCode,
};

const SERVER: &str = "https://mac-ani.jkcoxson.com";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SessionStatus {
    status: &'static str,
    dir_exists: bool,
    dir_private: bool,
    session_exists: bool,
    session_private: bool,
    session_valid: bool,
    anisette_state_exists: bool,
    error: Option<String>,
}

fn inspect_session(dir: &Path) -> SessionStatus {
    let dir_meta = match fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return SessionStatus {
                status: "missing",
                dir_exists: false,
                dir_private: false,
                session_exists: false,
                session_private: false,
                session_valid: false,
                anisette_state_exists: false,
                error: Some("session directory does not exist".to_string()),
            };
        }
        Err(_) => {
            return SessionStatus {
                status: "invalid",
                dir_exists: false,
                dir_private: false,
                session_exists: false,
                session_private: false,
                session_valid: false,
                anisette_state_exists: false,
                error: Some("cannot inspect session directory".to_string()),
            };
        }
    };

    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        return SessionStatus {
            status: "invalid",
            dir_exists: true,
            dir_private: false,
            session_exists: false,
            session_private: false,
            session_valid: false,
            anisette_state_exists: false,
            error: Some("session path must be a real directory".to_string()),
        };
    }

    let dir_private = dir_meta.permissions().mode() & 0o077 == 0;
    let anisette_state_exists =
        fs::symlink_metadata(dir.join("anisette_state")).is_ok_and(|m| m.is_file());
    let session_path = dir.join("apple-session.plist");
    let session_meta = match fs::symlink_metadata(&session_path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return SessionStatus {
                status: if dir_private { "missing" } else { "invalid" },
                dir_exists: true,
                dir_private,
                session_exists: false,
                session_private: false,
                session_valid: false,
                anisette_state_exists,
                error: Some(if dir_private {
                    "apple-session.plist not found".to_string()
                } else {
                    "session directory must be private (0700)".to_string()
                }),
            };
        }
        Err(_) => {
            return SessionStatus {
                status: "invalid",
                dir_exists: true,
                dir_private,
                session_exists: false,
                session_private: false,
                session_valid: false,
                anisette_state_exists,
                error: Some("cannot inspect apple-session.plist".to_string()),
            };
        }
    };

    let session_is_regular = !session_meta.file_type().is_symlink() && session_meta.is_file();
    let session_private = session_is_regular && session_meta.permissions().mode() & 0o077 == 0;
    if !dir_private {
        return SessionStatus {
            status: "invalid",
            dir_exists: true,
            dir_private: false,
            session_exists: true,
            session_private,
            session_valid: false,
            anisette_state_exists,
            error: Some("session directory must be private (0700)".to_string()),
        };
    }
    if !session_is_regular {
        return SessionStatus {
            status: "invalid",
            dir_exists: true,
            dir_private: true,
            session_exists: true,
            session_private: false,
            session_valid: false,
            anisette_state_exists,
            error: Some("apple-session.plist must be a regular file".to_string()),
        };
    }
    if !session_private {
        return SessionStatus {
            status: "invalid",
            dir_exists: true,
            dir_private: true,
            session_exists: true,
            session_private: false,
            session_valid: false,
            anisette_state_exists,
            error: Some("apple-session.plist must be private (0600)".to_string()),
        };
    }

    match plist::from_file::<_, (String, plist::Dictionary)>(&session_path) {
        Ok((email, spd)) if !email.trim().is_empty() && !spd.is_empty() => SessionStatus {
            status: "ok",
            dir_exists: true,
            dir_private: true,
            session_exists: true,
            session_private: true,
            session_valid: true,
            anisette_state_exists,
            error: None,
        },
        _ => SessionStatus {
            status: "invalid",
            dir_exists: true,
            dir_private: true,
            session_exists: true,
            session_private: true,
            session_valid: false,
            anisette_state_exists,
            error: Some("apple-session.plist has invalid or incomplete structure".to_string()),
        },
    }
}

fn format_session_status_human(status: &SessionStatus) -> String {
    if status.session_valid {
        if status.anisette_state_exists {
            "Apple session valid (anisette state present)".to_string()
        } else {
            "Apple session valid".to_string()
        }
    } else if status.status == "missing" {
        format!(
            "Apple session missing: {}",
            status.error.as_deref().unwrap_or("authorize first")
        )
    } else {
        format!(
            "Apple session invalid: {}",
            status.error.as_deref().unwrap_or("unknown error")
        )
    }
}

fn private_dir(dir: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other("session directory must be private (0700)"));
    }
    Ok(())
}

fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .mode(0o600)
        .create_new(true)
        .write(true)
        .open(path)?;
    if let Err(e) = file.write_all(data).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// The SideBox bridge records the exact Anisette endpoint used by the
/// original application. A missing file retains legacy standalone behavior.
/// This file contains a URL only, never a token, password or Apple session.
fn configured_anisette_url(dir: &Path) -> Result<String> {
    let path = dir.join(".sidebox-anisette-url");
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(SERVER.to_string()),
        Err(_) => return Err("could not check Anisette endpoint"),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() == 0
        || meta.len() > 2048 || meta.permissions().mode() & 0o077 != 0 {
        return Err("unsafe SideBox Anisette endpoint file");
    }
    let url = fs::read_to_string(&path).map_err(|_| "could not read SideBox Anisette endpoint")?;
    if url.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20)
        || url.contains(['@', '#']) {
        return Err("invalid SideBox Anisette endpoint");
    }
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else { return Err("invalid SideBox Anisette endpoint"); };
    let hostname = rest.split(['/', ':', '?']).next().unwrap_or_default();
    if hostname.is_empty() || (!secure && !matches!(hostname, "localhost" | "127.0.0.1")) {
        return Err("invalid SideBox Anisette endpoint");
    }
    Ok(url)
}

async fn account(dir: &Path, email: &str) -> Result<AppleAccount> {
    let endpoint = configured_anisette_url(dir)?;
    let provider = RemoteV3AnisetteProvider::default()
        .map_err(|_| "could not initialize Anisette")?
        .set_url(endpoint.as_str())
        .set_storage(Box::new(FsStorage::new(dir.into())));
    AppleAccount::builder(email)
        .anisette_provider(provider)
        .build()
        .await
        .map_err(|_| "could not start Apple account client")
}

type Result<T> = std::result::Result<T, &'static str>;

async fn authorize(dir: &Path, email: &str) -> Result<()> {
    let session = dir.join("apple-session.plist");
    if session.exists() {
        return Err("session already exists; use a new private directory to reauthorize");
    }
    let password = rpassword::prompt_password("Apple ID password: ")
        .map_err(|_| "a terminal is required for authorization")?;
    let mut account = account(dir, email).await?;
    let callback = async |params: TwoFactorCallbackParams| {
        if params.unknown {
            return Ok(TwoFactorCallbackResponse::SendToDevices);
        }
        let answer = rpassword::prompt_password("Apple verification code (empty to cancel): ")
            .unwrap_or_default();
        Ok(if answer.is_empty() {
            TwoFactorCallbackResponse::Abort
        } else {
            TwoFactorCallbackResponse::SubmitCode(answer)
        })
    };
    account
        .login(&password, callback)
        .await
        .map_err(|_| "Apple sign-in failed")?;
    drop(password);
    let data = account
        .spd
        .as_ref()
        .ok_or("no Apple session was returned")?;
    let mut bytes = Vec::new();
    let session_record = (email, data);
    plist::to_writer_binary(&mut bytes, &session_record).map_err(|_| "cannot encode session")?;
    write_new(&session, &bytes).map_err(|_| "cannot save private session")
}

async fn fetch(dir: &Path, team_id: &str, bundle: &str, destination: &Path) -> Result<()> {
    if destination.exists() {
        return Err("destination already exists");
    }
    let session_path = dir.join("apple-session.plist");
    let meta = fs::symlink_metadata(&session_path).map_err(|_| "authorize first")?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err("Apple session file is not private");
    }
    let (email, spd): (String, plist::Dictionary) =
        plist::from_file(session_path).map_err(|_| "cannot read saved Apple session")?;
    let mut account = account(dir, &email).await?;
    account.spd = Some(spd);
    let mut session = DeveloperSession::from_account(&mut account)
        .await
        .map_err(|_| "Apple session unavailable or expired; authorize again")?;
    let team = session
        .list_teams()
        .await
        .map_err(|_| "cannot list Apple developer teams")?
        .into_iter()
        .find(|team| team.team_id == team_id)
        .ok_or("configured Apple developer team was not found")?;
    let app = session
        .list_app_ids(&team, DeveloperDeviceType::Ios)
        .await
        .map_err(|_| "cannot list registered App IDs")?
        .app_ids
        .into_iter()
        .find(|app| app.identifier == bundle)
        .ok_or("bundle identifier is not registered on the configured team")?;
    let profile = session
        .download_team_provisioning_profile(&team, &app, DeveloperDeviceType::Ios)
        .await
        .map_err(|_| "Apple did not return a provisioning profile")?;
    write_new(destination, profile.encoded_profile.as_ref()).map_err(|_| "could not save profile")
}

async fn local_probe(record: &Path) -> Result<()> {
    use idevice::{
        IdeviceService, pairing_file::PairingFile, provider::TcpProvider,
        services::misagent::MisagentClient,
    };
    let pairing_file =
        PairingFile::read_from_file(record).map_err(|_| "invalid USB pairing record")?;
    let provider = TcpProvider {
        addr: std::net::Ipv4Addr::LOCALHOST.into(),
        scope_id: None,
        pairing_file,
        label: "Rebox".into(),
    };
    let mut client = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        MisagentClient::connect(&provider),
    )
    .await
    .map_err(|_| "local profile service connection timed out")?
    .map_err(|_| "could not open the local profile service")?;
    let profiles = tokio::time::timeout(std::time::Duration::from_secs(8), client.copy_all())
        .await
        .map_err(|_| "local profile query timed out")?
        .map_err(|_| "local profile service did not return a profile list")?;
    println!("Local profile service returned {} profiles", profiles.len());
    Ok(())
}

fn run_session_status(dir: &Path, json: bool) -> ExitCode {
    let status = inspect_session(dir);
    if json {
        match serde_json::to_string_pretty(&status) {
            Ok(out) => println!("{out}"),
            Err(_) => {
                println!(
                    r#"{{"status":"invalid","dir_exists":false,"dir_private":false,"session_exists":false,"session_private":false,"session_valid":false,"anisette_state_exists":false,"error":"cannot serialize session status"}}"#
                );
                return ExitCode::FAILURE;
            }
        }
    } else if status.session_valid {
        println!("{}", format_session_status_human(&status));
    } else {
        eprintln!("{}", format_session_status_human(&status));
    }
    if status.session_valid {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, dir] if cmd == "session-status" => return run_session_status(Path::new(dir), false),
        [cmd, dir, flag] if cmd == "session-status" && flag == "--json" => {
            return run_session_status(Path::new(dir), true);
        }
        [cmd, flag, dir] if cmd == "session-status" && flag == "--json" => {
            return run_session_status(Path::new(dir), true);
        }
        _ => {}
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    if isideload::init().is_err() {
        eprintln!("could not initialize signing library");
        return ExitCode::FAILURE;
    }
    let result = match args.as_slice() {
        [cmd, record] if cmd == "local-probe" => local_probe(Path::new(record)).await,
        [command, dir, email] if command == "authorize" => {
            let dir = Path::new(dir);
            match private_dir(dir) {
                Ok(()) => authorize(dir, email).await,
                Err(_) => Err("cannot open a private session directory"),
            }
        }
        [command, dir, team, bundle, output] if command == "fetch" => {
            let dir = Path::new(dir);
            match private_dir(dir) {
                Ok(()) => fetch(dir, team, bundle, Path::new(output)).await,
                Err(_) => Err("cannot open a private session directory"),
            }
        }
        _ => Err(
            "usage: rebox-apple authorize DIR EMAIL | fetch DIR TEAM BUNDLE OUTPUT | session-status DIR [--json]",
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn inherits_sidebox_anisette_url_without_secret_logging() {
        let dir = unique_temp_dir("sidebox-url");
        fs::create_dir(&dir).unwrap();
        assert_eq!(configured_anisette_url(&dir).unwrap(), SERVER);
        let path = dir.join(".sidebox-anisette-url");
        fs::write(&path, b"https://anisette.example.org/v3").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(configured_anisette_url(&dir).unwrap(), "https://anisette.example.org/v3");
        fs::write(&path, b"http://example.org/").unwrap();
        assert!(configured_anisette_url(&dir).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!(
            "rebox-apple-test-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn write_session_plist(path: &Path, email: &str, spd: plist::Dictionary, mode: u32) {
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &(email, spd)).unwrap();
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

    #[test]
    fn missing_dir_reports_missing() {
        let dir = unique_temp_dir("missing");
        let st = inspect_session(&dir);
        assert_eq!(st.status, "missing");
        assert!(!st.dir_exists);
        assert!(!st.session_valid);
    }

    #[test]
    fn insecure_dir_and_file_rejected() {
        let dir = unique_temp_dir("insecure");
        fs::DirBuilder::new().mode(0o755).create(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        let mut spd = plist::Dictionary::new();
        spd.insert(
            "token".into(),
            plist::Value::String("SUPER_SECRET_TOKEN_XYZ".into()),
        );
        let session = dir.join("apple-session.plist");
        write_session_plist(&session, "secret@example.com", spd.clone(), 0o600);

        let st = inspect_session(&dir);
        assert_eq!(st.status, "invalid");
        assert!(!st.dir_private);
        assert!(!st.session_valid);

        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&session, fs::Permissions::from_mode(0o644)).unwrap();
        let st2 = inspect_session(&dir);
        assert_eq!(st2.status, "invalid");
        assert!(st2.dir_private);
        assert!(!st2.session_private);
        assert!(!st2.session_valid);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_session_rejected() {
        let dir = unique_temp_dir("symlink");
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let real = dir.join("real.plist");
        let mut spd = plist::Dictionary::new();
        spd.insert("k".into(), plist::Value::String("v".into()));
        write_session_plist(&real, "user@example.com", spd, 0o600);
        symlink(&real, dir.join("apple-session.plist")).unwrap();

        let st = inspect_session(&dir);
        assert_eq!(st.status, "invalid");
        assert!(!st.session_valid);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_session_passes_and_never_leaks_secrets() {
        let dir = unique_temp_dir("valid");
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();

        let mut spd = plist::Dictionary::new();
        spd.insert(
            "GSIdmsToken".into(),
            plist::Value::String("TOP_SECRET_SPD_TOKEN_999".into()),
        );
        write_session_plist(
            &dir.join("apple-session.plist"),
            "private-user@example.com",
            spd,
            0o600,
        );
        fs::write(dir.join("anisette_state"), b"state").unwrap();

        let st = inspect_session(&dir);
        assert_eq!(st.status, "ok");
        assert!(st.dir_private);
        assert!(st.session_exists);
        assert!(st.session_private);
        assert!(st.session_valid);
        assert!(st.anisette_state_exists);
        assert!(st.error.is_none());

        let json = serde_json::to_string(&st).unwrap();
        let human = format_session_status_human(&st);
        for out in [&json, &human] {
            assert!(!out.contains("TOP_SECRET_SPD_TOKEN_999"));
            assert!(!out.contains("GSIdmsToken"));
            assert!(!out.contains("private-user@example.com"));
        }

        // Empty SPD dict must fail validation.
        write_session_plist(
            &dir.join("apple-session.plist"),
            "private-user@example.com",
            plist::Dictionary::new(),
            0o600,
        );
        let bad = inspect_session(&dir);
        assert_eq!(bad.status, "invalid");
        assert!(!bad.session_valid);

        let _ = fs::remove_dir_all(&dir);
    }
}
