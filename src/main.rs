mod profile;

use serde::Deserialize;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    backend: PathBuf,
    renew_before_hours: u64,
    apps: Vec<String>,
}

impl Config {
    fn read(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        if !config.backend.is_absolute() || !(1..=168).contains(&config.renew_before_hours) {
            return Err(
                "backend must be an absolute path and renew_before_hours must be 1–168".into(),
            );
        }
        if config.apps.is_empty()
            || config.apps.iter().any(|id| {
                !id.contains('.')
                    || id.split('.').any(|part| {
                        part.is_empty()
                            || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
            })
        {
            return Err("apps must contain explicit bundle identifiers".into());
        }
        let unique: std::collections::HashSet<_> = config.apps.iter().collect();
        if unique.len() != config.apps.len() {
            return Err("duplicate bundle identifier".into());
        }
        Ok(config)
    }
}

fn invoke(backend: &Path, action: &str, app: &str) -> io::Result<Option<u64>> {
    let mut child = Command::new(backend)
        .args([action, app])
        .stdin(Stdio::null())
        .stdout(if action == "expiry" {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "backend timed out"));
        }
        thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        return Err(io::Error::other(format!("backend returned {status}")));
    }
    let Some(stdout) = child.stdout.take() else {
        return Ok(None);
    };
    use io::Read;
    let mut answer = String::new();
    stdout.take(65).read_to_string(&mut answer)?;
    answer
        .trim()
        .parse()
        .map(Some)
        .map_err(|_| io::Error::other("expected one Unix timestamp"))
}

fn expiry(config: &Config, app: &str) -> io::Result<u64> {
    invoke(&config.backend, "expiry", app)?
        .ok_or_else(|| io::Error::other("backend did not report an expiration"))
}

fn is_due(expiration: u64, now: u64, hours: u64) -> bool {
    expiration <= now.saturating_add(hours * 3600)
}

fn run(mode: &str, config: &Config) -> Result<bool, Box<dyn std::error::Error>> {
    let mut all_ok = true;
    for app in &config.apps {
        let result = (|| -> Result<String, Box<dyn std::error::Error>> {
            let before = expiry(config, app)?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if !is_due(before, now, config.renew_before_hours) {
                return Ok(format!("signature valid until {before}"));
            }
            if mode == "check" {
                return Ok(format!("renewal due, current expiry {before}"));
            }
            let _ = invoke(&config.backend, "renew", app)?;
            let after = expiry(config, app)?;
            if after <= before || after <= now {
                return Err("no newer active profile was verified".into());
            }
            Ok(format!("verified new expiry {after}"))
        })();
        match result {
            Ok(message) => println!("{app}: {message}"),
            Err(error) => {
                eprintln!("{app}: {error}");
                all_ok = false;
            }
        }
    }
    Ok(all_ok)
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args_os().skip(1).collect();
    #[cfg(not(target_os = "ios"))]
    if let [mode, openssl, provision, old, new, device, rest @ ..] = args.as_slice()
        && mode == "apply-usb"
        && rest.len() <= 1
        && let Some(device) = device.to_str()
    {
        return match profile::apply_usb(
            Path::new(openssl),
            Path::new(provision),
            Path::new(old),
            Path::new(new),
            device,
            rest.first().map(Path::new),
        ) {
            Ok(date) => {
                println!("{date}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("rebox: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if let [mode, openssl, old, new, device] = args.as_slice()
        && mode == "check-replacement"
        && let Some(device) = device.to_str()
    {
        return match profile::check_replacement(
            Path::new(openssl),
            Path::new(old),
            Path::new(new),
            device,
        ) {
            Ok(date) => {
                println!("{date}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("rebox: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if let [mode, openssl, input] = args.as_slice()
        && mode == "profile-expiry"
    {
        return match profile::expiration(Path::new(openssl), Path::new(input)) {
            Ok(date) => {
                println!("{date}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("rebox: {error}");
                ExitCode::FAILURE
            }
        };
    }
    if let [mode, config] = args.as_slice()
        && let Some(mode @ ("check" | "run")) = mode.to_str()
    {
        let config = Path::new(config);
        let result = (|| {
            use std::os::unix::fs::OpenOptionsExt;
            let lock = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(config.with_extension("lock"))?;
            lock.try_lock()?;
            run(mode, &Config::read(config)?)
        })();
        match result {
            Ok(true) => return ExitCode::SUCCESS,
            Ok(false) => return ExitCode::FAILURE,
            Err(e) => eprintln!("rebox: {e}"),
        }
    } else {
        eprintln!("Usage: rebox <check|run> <config.toml>");
        eprintln!("       rebox profile-expiry <openssl-path> <mobileprovision>");
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::is_due;

    #[test]
    fn signature_is_due_at_threshold_or_after_expiration() {
        assert!(!is_due(10_000, 6_399, 1));
        assert!(is_due(10_000, 6_400, 1));
        assert!(is_due(9_999, 10_000, 1));
    }
}
