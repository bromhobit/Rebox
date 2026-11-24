use std::{
    io,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

fn decode(openssl: &Path, profile: &Path) -> Result<plist::Dictionary, Box<dyn std::error::Error>> {
    if !openssl.is_absolute() {
        return Err("OpenSSL executable path must be absolute".into());
    }
    if !profile.is_file() || profile.metadata()?.len() > 8 * 1024 * 1024 {
        return Err("expected a provisioning profile file of at most 8 MiB".into());
    }

    // Verify CMS integrity without making a claim about the signer's trust chain.
    let decoded = Command::new(openssl)
        .args([
            "cms",
            "-verify",
            "-inform",
            "DER",
            "-binary",
            "-noverify",
            "-in",
        ])
        .arg(profile)
        .stdin(Stdio::null())
        .output()?;
    if !decoded.status.success() {
        return Err("could not decode provisioning profile CMS".into());
    }

    Ok(plist::from_bytes(&decoded.stdout)?)
}

fn expiry(data: &plist::Dictionary) -> Result<u64, Box<dyn std::error::Error>> {
    let date = data
        .get("ExpirationDate")
        .and_then(plist::Value::as_date)
        .ok_or_else(|| io::Error::other("profile has no ExpirationDate"))?;
    Ok(SystemTime::from(date).duration_since(UNIX_EPOCH)?.as_secs())
}

pub fn expiration(openssl: &Path, profile: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    expiry(&decode(openssl, profile)?)
}

fn entitlements(profile: &plist::Dictionary) -> io::Result<&plist::Dictionary> {
    profile
        .get("Entitlements")
        .and_then(plist::Value::as_dictionary)
        .ok_or_else(|| io::Error::other("profile has no entitlements"))
}

fn array<'a>(profile: &'a plist::Dictionary, key: &str) -> io::Result<&'a [plist::Value]> {
    profile
        .get(key)
        .and_then(plist::Value::as_array)
        .map(Vec::as_slice)
        .filter(|values| !values.is_empty())
        .ok_or_else(|| io::Error::other(format!("profile has no {key}")))
}

pub fn check_replacement(
    openssl: &Path,
    old: &Path,
    new: &Path,
    device: &str,
) -> Result<u64, Box<dyn std::error::Error>> {
    let old = decode(openssl, old)?;
    let new = decode(openssl, new)?;
    let old_entitlements = entitlements(&old)?;
    let new_entitlements = entitlements(&new)?;
    let app = old_entitlements
        .get("application-identifier")
        .and_then(plist::Value::as_string)
        .ok_or_else(|| io::Error::other("old profile has no application identifier"))?;
    if app.contains('*') || old_entitlements != new_entitlements {
        return Err("profile entitlements changed or app identifier is not explicit".into());
    }
    if array(&old, "TeamIdentifier")? != array(&new, "TeamIdentifier")? {
        return Err("profile teams do not match".into());
    }
    for profile in [&old, &new] {
        if !array(profile, "ProvisionedDevices")?.iter().any(|entry| {
            entry
                .as_string()
                .is_some_and(|id| id.eq_ignore_ascii_case(device))
        }) {
            return Err("profile does not include the target device".into());
        }
    }
    let new_certs = array(&new, "DeveloperCertificates")?;
    if !array(&old, "DeveloperCertificates")?
        .iter()
        .all(|certificate| certificate.as_data().is_some() && new_certs.contains(certificate))
    {
        return Err("replacement does not retain the old signing certificates".into());
    }
    let new_expiry = expiry(&new)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if new_expiry <= expiry(&old)? || new_expiry <= now {
        return Err("replacement profile does not extend expiration".into());
    }
    Ok(new_expiry)
}

#[cfg(not(target_os = "ios"))]
pub fn apply_usb(
    openssl: &Path,
    provision: &Path,
    old: &Path,
    new: &Path,
    device: &str,
    rust_installer: Option<&Path>,
) -> Result<u64, Box<dyn std::error::Error>> {
    if !provision.is_absolute() || rust_installer.is_some_and(|tool| !tool.is_absolute()) {
        return Err("USB tool paths must be absolute".into());
    }
    let expires = check_replacement(openssl, old, new, device)?;
    let work = tempfile::tempdir()?;
    let old_uuid = profile_uuid(openssl, old)?;
    let new_uuid = profile_uuid(openssl, new)?;
    if old_uuid == new_uuid {
        return Err("replacement should have a new profile UUID".into());
    }
    let run = |command: &str, args: &[&std::ffi::OsStr]| -> io::Result<()> {
        let status = Command::new(provision)
            .args(["-u", device, command])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!(
                "ideviceprovision {command} failed: {status}"
            )));
        }
        Ok(())
    };
    // Require the baseline to match the device's currently installed profile.
    run("copy", &[old_uuid.as_ref(), work.path().as_os_str()])?;
    let installed_old = work.path().join(format!("{old_uuid}.mobileprovision"));
    if std::fs::read(installed_old)? != std::fs::read(old)? {
        return Err("installed profile differs from the supplied baseline".into());
    }
    let installed_new = work.path().join(format!("{new_uuid}.mobileprovision"));
    let expected = std::fs::read(new)?;
    let copy_new = || run("copy", &[new_uuid.as_ref(), work.path().as_os_str()]);
    if copy_new().is_ok() && std::fs::read(&installed_new).is_ok_and(|bytes| bytes == expected) {
        return Ok(expires);
    }
    if let Some(installer) = rust_installer {
        let status = Command::new(installer)
            .args(["--udid", device, "misagent", "install"])
            .arg(new)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other(format!("Rust USB installer failed: {status}")).into());
        }
    } else {
        run("install", &[new.as_os_str()])?;
    }
    copy_new()?;
    if std::fs::read(installed_new)? != expected {
        return Err("could not verify the installed profile".into());
    }
    Ok(expires)
}

#[cfg(not(target_os = "ios"))]
fn profile_uuid(openssl: &Path, profile: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let data = decode(openssl, profile)?;
    let uuid = data
        .get("UUID")
        .and_then(plist::Value::as_string)
        .filter(|id| id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .ok_or("profile UUID is missing or invalid")?;
    Ok(uuid.to_owned())
}
