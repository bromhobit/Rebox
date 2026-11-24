#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_RENEW_BEFORE_HOURS: u64 = 48;

const FALLBACK_CONTROLLER_PATHS: &[&str] = &[
    "/var/jb/usr/libexec/rebox/rebox-controller",
    "/var/mobile/Library/Rebox/bin/rebox-controller",
];

fn default_renew_before_hours() -> u64 {
    DEFAULT_RENEW_BEFORE_HOURS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Status,
    Refresh,
}

impl Operation {
    pub fn as_arg(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Refresh => "refresh",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusTone {
    Green,
    Amber,
    Red,
    Neutral,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ControllerReport {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub config_exists: bool,
    #[serde(default)]
    pub apple_session_exists: bool,
    #[serde(default)]
    pub apple_session_valid: bool,
    #[serde(default)]
    pub daemon_installed: bool,
    #[serde(default)]
    pub daemon_enabled: bool,
    #[serde(default = "default_renew_before_hours")]
    pub renew_before_hours: u64,
    #[serde(default)]
    pub checked_at_unix: u64,
    #[serde(default)]
    pub apps: Vec<ControllerApp>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ControllerApp {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bundle_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    #[serde(default)]
    pub hours_remaining: Option<u64>,
    #[serde(default)]
    pub needs_renewal: bool,
    #[serde(default)]
    pub renewed: bool,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppCardViewModel {
    pub name: String,
    pub bundle_id: String,
    pub badge_text: String,
    pub tone: StatusTone,
    pub remaining_text: String,
    pub expiration_text: String,
    pub detail_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadinessRowViewModel {
    pub title: String,
    pub value: String,
    pub tone: StatusTone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiState {
    pub header_subtitle: String,
    pub overall_tone: StatusTone,
    pub apps: Vec<AppCardViewModel>,
    pub empty_apps_title: String,
    pub empty_apps_subtitle: String,
    pub readiness: Vec<ReadinessRowViewModel>,
    pub diagnostics_title: String,
    pub diagnostics_tone: StatusTone,
    pub diagnostics_body: String,
    pub setup_hint: Option<String>,
}

impl UiState {
    pub fn initial_loading() -> Self {
        Self {
            header_subtitle: "Проверяем состояние подписей…".to_string(),
            overall_tone: StatusTone::Neutral,
            apps: Vec::new(),
            empty_apps_title: "Загрузка списка приложений…".to_string(),
            empty_apps_subtitle: "Считываем профили подписи с устройства.".to_string(),
            readiness: vec![
                ReadinessRowViewModel {
                    title: "Сессия Apple ID".to_string(),
                    value: "Проверка…".to_string(),
                    tone: StatusTone::Neutral,
                },
                ReadinessRowViewModel {
                    title: "Фоновая служба".to_string(),
                    value: "Проверка…".to_string(),
                    tone: StatusTone::Neutral,
                },
                ReadinessRowViewModel {
                    title: "Окно продления".to_string(),
                    value: format!("За {DEFAULT_RENEW_BEFORE_HOURS} ч до истечения"),
                    tone: StatusTone::Neutral,
                },
            ],
            diagnostics_title: "Состояние операции".to_string(),
            diagnostics_tone: StatusTone::Neutral,
            diagnostics_body: "Выполняется первичная проверка конфигурации и профилей…".to_string(),
            setup_hint: None,
        }
    }
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn resolve_controller_from(
    current_exe: Option<&Path>,
    fallbacks: &[&Path],
) -> Option<PathBuf> {
    if let Some(exe) = current_exe
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("rebox-controller");
        if is_executable_file(&sibling) {
            return Some(sibling);
        }
    }
    for candidate in fallbacks {
        if is_executable_file(candidate) {
            return Some((*candidate).to_path_buf());
        }
    }
    None
}

pub fn resolve_controller_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok();
    let fallbacks: Vec<&Path> = FALLBACK_CONTROLLER_PATHS.iter().map(Path::new).collect();
    resolve_controller_from(exe.as_deref(), &fallbacks)
}

pub fn execute_operation(operation: Operation, tz_offset_secs: impl Fn(u64) -> i64) -> UiState {
    let controller = resolve_controller_path();
    execute_operation_with(controller.as_deref(), operation, tz_offset_secs)
}

pub fn execute_operation_with(
    controller: Option<&Path>,
    operation: Operation,
    tz_offset_secs: impl Fn(u64) -> i64,
) -> UiState {
    let Some(controller) = controller else {
        return missing_controller_state();
    };

    let output = Command::new(controller)
        .args([operation.as_arg(), "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if let Ok(report) = parse_controller_json(&stdout) {
                return build_ui_state(&report, &tz_offset_secs);
            }

            // Graceful fallback if an older non-JSON controller binary is installed.
            if !out.status.success() && stderr.contains("Usage:") {
                if let Ok(legacy) = Command::new(controller)
                    .arg(operation.as_arg())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                {
                    let l_out = String::from_utf8_lossy(&legacy.stdout);
                    let l_err = String::from_utf8_lossy(&legacy.stderr);
                    return fallback_plain_state(
                        operation,
                        legacy.status.success(),
                        &l_out,
                        &l_err,
                        now_unix_secs(),
                        &tz_offset_secs,
                    );
                }
            }

            fallback_plain_state(
                operation,
                out.status.success(),
                &stdout,
                &stderr,
                now_unix_secs(),
                &tz_offset_secs,
            )
        }
        Err(err) => UiState {
            header_subtitle: "Ошибка запуска службы".to_string(),
            overall_tone: StatusTone::Red,
            apps: Vec::new(),
            empty_apps_title: "Не удалось запустить контроллер".to_string(),
            empty_apps_subtitle: format!("{}", controller.display()),
            readiness: unknown_readiness(),
            diagnostics_title: "Ошибка запуска".to_string(),
            diagnostics_tone: StatusTone::Red,
            diagnostics_body: format!(
                "Не удалось выполнить rebox-controller: {}",
                truncate_chars(&err.to_string(), 300)
            ),
            setup_hint: Some(
                "Проверьте права исполнения и подпись (entitlements) установленного пакета Rebox."
                    .to_string(),
            ),
        },
    }
}

pub fn parse_controller_json(raw: &str) -> Result<ControllerReport, serde_json::Error> {
    let trimmed = raw.trim();
    serde_json::from_str::<ControllerReport>(trimmed)
}

fn unknown_readiness() -> Vec<ReadinessRowViewModel> {
    vec![
        ReadinessRowViewModel {
            title: "Сессия Apple ID".to_string(),
            value: "Нет данных".to_string(),
            tone: StatusTone::Neutral,
        },
        ReadinessRowViewModel {
            title: "Фоновая служба".to_string(),
            value: "Нет данных".to_string(),
            tone: StatusTone::Neutral,
        },
        ReadinessRowViewModel {
            title: "Окно продления".to_string(),
            value: format!("За {DEFAULT_RENEW_BEFORE_HOURS} ч до истечения"),
            tone: StatusTone::Neutral,
        },
    ]
}

fn missing_controller_state() -> UiState {
    UiState {
        header_subtitle: "Служба не найдена".to_string(),
        overall_tone: StatusTone::Red,
        apps: Vec::new(),
        empty_apps_title: "Контроллер не установлен".to_string(),
        empty_apps_subtitle: "Не найден исполняемый файл rebox-controller.".to_string(),
        readiness: vec![
            ReadinessRowViewModel {
                title: "Сессия Apple ID".to_string(),
                value: "Нет данных".to_string(),
                tone: StatusTone::Neutral,
            },
            ReadinessRowViewModel {
                title: "Фоновая служба".to_string(),
                value: "Не установлена".to_string(),
                tone: StatusTone::Red,
            },
            ReadinessRowViewModel {
                title: "Окно продления".to_string(),
                value: format!("За {DEFAULT_RENEW_BEFORE_HOURS} ч до истечения"),
                tone: StatusTone::Neutral,
            },
        ],
        diagnostics_title: "Требуется установка".to_string(),
        diagnostics_tone: StatusTone::Red,
        diagnostics_body: "Приложение не нашло rebox-controller ни рядом с исполняемым файлом, ни в /var/jb/usr/libexec/rebox/, ни в /var/mobile/Library/Rebox/bin/.".to_string(),
        setup_hint: Some(
            "Установите полный rootless .deb-пакет Rebox через Sileo/Zebra или TrollStore IPA со встроенными утилитами."
                .to_string(),
        ),
    }
}

fn fallback_plain_state(
    operation: Operation,
    success: bool,
    stdout: &str,
    stderr: &str,
    now: u64,
    tz_offset_secs: &impl Fn(u64) -> i64,
) -> UiState {
    let out_trimmed = stdout.trim();
    let err_trimmed = stderr.trim();
    let time_str = format_hhmm(now, tz_offset_secs(now));

    let (tone, title, body) = if success {
        let msg = if out_trimmed.is_empty() {
            match operation {
                Operation::Refresh => "Операция продления завершена.".to_string(),
                Operation::Status => "Проверка статуса завершена.".to_string(),
            }
        } else {
            truncate_chars(out_trimmed, 500)
        };
        (StatusTone::Green, "Результат операции".to_string(), msg)
    } else {
        let msg = if !err_trimmed.is_empty() {
            truncate_chars(err_trimmed, 500)
        } else if !out_trimmed.is_empty() {
            truncate_chars(out_trimmed, 500)
        } else {
            "Контроллер вернул ошибку без описания.".to_string()
        };
        (StatusTone::Red, "Ошибка операции".to_string(), msg)
    };

    UiState {
        header_subtitle: format!("Проверено в {time_str}"),
        overall_tone: tone,
        apps: Vec::new(),
        empty_apps_title: if success {
            "Получен текстовый ответ службы".to_string()
        } else {
            "Нет структурированных данных".to_string()
        },
        empty_apps_subtitle: "Обновите контроллер Rebox для отображения карточек приложений."
            .to_string(),
        readiness: unknown_readiness(),
        diagnostics_title: title,
        diagnostics_tone: tone,
        diagnostics_body: body,
        setup_hint: None,
    }
}

pub fn build_ui_state(
    report: &ControllerReport,
    tz_offset_secs: &impl Fn(u64) -> i64,
) -> UiState {
    let now = if report.checked_at_unix > 0 {
        report.checked_at_unix
    } else {
        now_unix_secs()
    };
    let time_str = format_hhmm(now, tz_offset_secs(now));

    let overall_tone = match report.status.as_str() {
        "ok" => StatusTone::Green,
        "expiring_soon" | "needs_setup" => StatusTone::Amber,
        "expired" | "error" => StatusTone::Red,
        _ => StatusTone::Neutral,
    };

    let header_subtitle = match report.status.as_str() {
        "ok" => format!("Всё в порядке · {time_str}"),
        "expiring_soon" => format!("Скоро продление · {time_str}"),
        "expired" => format!("Есть истёкшие подписи · {time_str}"),
        "needs_setup" => format!("Требуется настройка · {time_str}"),
        "error" => format!("Есть ошибки · {time_str}"),
        _ => format!("Проверено в {time_str}"),
    };

    let apps: Vec<AppCardViewModel> = report
        .apps
        .iter()
        .map(|app| build_app_card(app, now, tz_offset_secs))
        .collect();

    let (empty_apps_title, empty_apps_subtitle) = if !report.config_exists {
        (
            "Приложения не настроены".to_string(),
            "Файл config.plist пока отсутствует на устройстве.".to_string(),
        )
    } else {
        (
            "Список приложений пуст".to_string(),
            "Добавьте bundle ID приложений в /var/mobile/Library/Rebox/config.plist."
                .to_string(),
        )
    };

    let apple_row = if report.apple_session_exists && report.apple_session_valid {
        ReadinessRowViewModel {
            title: "Сессия Apple ID".to_string(),
            value: "Активна".to_string(),
            tone: StatusTone::Green,
        }
    } else {
        ReadinessRowViewModel {
            title: "Сессия Apple ID".to_string(),
            value: "Требуется вход".to_string(),
            tone: StatusTone::Red,
        }
    };

    let daemon_row = if report.daemon_installed && report.daemon_enabled {
        ReadinessRowViewModel {
            title: "Фоновая служба".to_string(),
            value: "Включена".to_string(),
            tone: StatusTone::Green,
        }
    } else if report.daemon_installed {
        ReadinessRowViewModel {
            title: "Фоновая служба".to_string(),
            value: "Отключена".to_string(),
            tone: StatusTone::Amber,
        }
    } else {
        ReadinessRowViewModel {
            title: "Фоновая служба".to_string(),
            value: "Не установлена".to_string(),
            tone: StatusTone::Amber,
        }
    };

    let window_hours = if report.renew_before_hours == 0 {
        DEFAULT_RENEW_BEFORE_HOURS
    } else {
        report.renew_before_hours
    };
    let window_row = ReadinessRowViewModel {
        title: "Окно продления".to_string(),
        value: format!("За {window_hours} ч до истечения"),
        tone: StatusTone::Neutral,
    };

    let diagnostics_title = match (report.command.as_str(), overall_tone) {
        ("refresh", StatusTone::Green) => "Продление выполнено".to_string(),
        ("refresh", StatusTone::Red) => "Ошибка продления".to_string(),
        ("refresh", StatusTone::Amber) => "Продление требует внимания".to_string(),
        (_, StatusTone::Green) => "Статус системы".to_string(),
        (_, StatusTone::Amber) => "Требуется внимание".to_string(),
        (_, StatusTone::Red) => "Диагностика ошибки".to_string(),
        _ => "Результат проверки".to_string(),
    };

    let mut body_parts = Vec::new();
    let msg = report.message.trim();
    if !msg.is_empty() {
        body_parts.push(truncate_chars(msg, 450));
    }
    if let Some(err) = report.last_error.as_deref().map(str::trim)
        && !err.is_empty()
        && err != msg
    {
        body_parts.push(format!("Детали: {}", truncate_chars(err, 450)));
    }
    if body_parts.is_empty() {
        body_parts.push(match overall_tone {
            StatusTone::Green => "Все отслеживаемые подписи актуальны.".to_string(),
            StatusTone::Amber => "Проверьте настройки автопродления.".to_string(),
            StatusTone::Red => "Не удалось подтвердить статус подписей.".to_string(),
            StatusTone::Neutral => "Проверка завершена.".to_string(),
        });
    }

    let mut hints = Vec::new();
    if !report.config_exists {
        hints.push(
            "• Конфигурация: создайте /var/mobile/Library/Rebox/config.plist через setup-device.sh или rebox-controller init-from-installed."
                .to_string(),
        );
    }
    if !report.apple_session_exists || !report.apple_session_valid {
        hints.push(
            "• Apple ID: выполните вход через rebox-apple authorize /var/mobile/Library/Rebox <email> или скопируйте сессию через setup-device.sh."
                .to_string(),
        );
    }
    if !report.daemon_installed {
        hints.push(
            "• Автопродление: установите rootless .deb-пакет для запуска фонового LaunchDaemon каждые 6 часов."
                .to_string(),
        );
    } else if !report.daemon_enabled {
        hints.push(
            "• Фоновая служба отключена в plist — переустановите .deb-пакет или включите com.bromhobit.rebox.plist."
                .to_string(),
        );
    }

    let setup_hint = if hints.is_empty() {
        None
    } else {
        Some(hints.join("\n"))
    };

    UiState {
        header_subtitle,
        overall_tone,
        apps,
        empty_apps_title,
        empty_apps_subtitle,
        readiness: vec![apple_row, daemon_row, window_row],
        diagnostics_title,
        diagnostics_tone: overall_tone,
        diagnostics_body: body_parts.join("\n\n"),
        setup_hint,
    }
}

fn build_app_card(
    app: &ControllerApp,
    now_unix: u64,
    tz_offset_secs: &impl Fn(u64) -> i64,
) -> AppCardViewModel {
    let (tone, badge_text) = match app.status.as_str() {
        "ok" => (
            StatusTone::Green,
            if app.renewed {
                "Продлён".to_string()
            } else {
                "Действует".to_string()
            },
        ),
        "expiring_soon" => (StatusTone::Amber, "Скоро истечёт".to_string()),
        "expired" => (StatusTone::Red, "Истёк".to_string()),
        "error" => (StatusTone::Red, "Ошибка".to_string()),
        _ if app.needs_renewal => (StatusTone::Amber, "Нужно продлить".to_string()),
        _ => (StatusTone::Red, "Ошибка".to_string()),
    };

    let display_name = if !app.name.trim().is_empty() {
        app.name.trim().to_string()
    } else if !app.bundle_id.trim().is_empty() {
        app.bundle_id.trim().to_string()
    } else {
        "Приложение".to_string()
    };

    let remaining_text = format_remaining(app.expires_at_unix, now_unix, app.hours_remaining);

    let expiration_text = match app.expires_at_unix {
        Some(exp) if exp > 0 => {
            let formatted = format_date_ru(exp, tz_offset_secs(exp));
            if exp <= now_unix {
                format!("Истёк: {formatted}")
            } else {
                format!("Действует до: {formatted}")
            }
        }
        _ => "Дата окончания недоступна".to_string(),
    };

    let msg = app.message.trim();
    let detail_text = if app.status == "error" && !msg.is_empty() {
        Some(truncate_chars(msg, 220))
    } else if app.renewed {
        Some("Профиль успешно обновлён и подтверждён iOS.".to_string())
    } else if app.status == "expiring_soon" {
        Some("Срок меньше окна продления — можно обновить сейчас.".to_string())
    } else {
        None
    };

    AppCardViewModel {
        name: display_name,
        bundle_id: app.bundle_id.clone(),
        badge_text,
        tone,
        remaining_text,
        expiration_text,
        detail_text,
    }
}

pub fn format_remaining(
    expires_at_unix: Option<u64>,
    now_unix: u64,
    fallback_hours: Option<u64>,
) -> String {
    if let Some(exp) = expires_at_unix
        && exp > 0
    {
        if exp <= now_unix {
            let ago = now_unix - exp;
            if ago < 60 {
                return "Истёк только что".to_string();
            } else if ago < 3600 {
                return format!("Истёк {} мин. назад", ago / 60);
            } else if ago < 86_400 {
                return format!("Истёк {} ч. назад", ago / 3600);
            } else {
                return format!("Истёк {} дн. назад", ago / 86_400);
            }
        }
        let rem = exp - now_unix;
        let days = rem / 86_400;
        let hours = (rem % 86_400) / 3600;
        let mins = (rem % 3600) / 60;
        if days > 0 {
            return format!("Осталось: {days} дн. {hours} ч.");
        } else if hours > 0 {
            return format!("Осталось: {hours} ч. {mins} мин.");
        } else if mins > 0 {
            return format!("Осталось: {mins} мин.");
        } else {
            return "Осталось менее 1 мин.".to_string();
        }
    }

    if let Some(h) = fallback_hours {
        if h == 0 {
            return "Срок действия истёк".to_string();
        } else if h >= 24 {
            return format!("Осталось: ~{} дн. {} ч.", h / 24, h % 24);
        } else {
            return format!("Осталось: ~{h} ч.");
        }
    }

    "Срок неизвестен".to_string()
}

/// Converts Unix timestamp + timezone offset (seconds) into `(year, month, day, hour, minute, second)`.
pub fn unix_to_ymdhms(unix_secs: i64, tz_offset_secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let local = unix_secs.saturating_add(tz_offset_secs);
    let days = local.div_euclid(86_400);
    let sod = local.rem_euclid(86_400) as u32;
    let hour = sod / 3600;
    let min = (sod % 3600) / 60;
    let sec = sod % 60;

    // Howard Hinnant's civil_from_days algorithm (proleptic Gregorian calendar).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year, m as u32, d as u32, hour, min, sec)
}

pub fn format_date_ru(unix_secs: u64, tz_offset_secs: i64) -> String {
    const MONTHS: [&str; 12] = [
        "янв.",
        "февр.",
        "мар.",
        "апр.",
        "мая",
        "июн.",
        "июл.",
        "авг.",
        "сент.",
        "окт.",
        "нояб.",
        "дек.",
    ];
    let (year, month, day, hour, min, _) = unix_to_ymdhms(unix_secs as i64, tz_offset_secs);
    let m_str = MONTHS.get(month.saturating_sub(1) as usize).copied().unwrap_or("");
    format!("{day} {m_str} {year}, {hour:02}:{min:02}")
}

pub fn format_hhmm(unix_secs: u64, tz_offset_secs: i64) -> String {
    let (_, _, _, hour, min, _) = unix_to_ymdhms(unix_secs as i64, tz_offset_secs);
    format!("{hour:02}:{min:02}")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn resolves_controller_in_priority_order() {
        let base = std::env::temp_dir().join(format!(
            "rebox-ui-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        let app_dir = base.join("App.app");
        let jb_dir = base.join("jb");
        let mob_dir = base.join("mobile");
        fs::create_dir_all(&app_dir).unwrap();
        fs::create_dir_all(&jb_dir).unwrap();
        fs::create_dir_all(&mob_dir).unwrap();

        let exe_path = app_dir.join("Rebox");
        let sibling = app_dir.join("rebox-controller");
        let jb_bin = jb_dir.join("rebox-controller");
        let mob_bin = mob_dir.join("rebox-controller");

        for p in [&mob_bin, &jb_bin, &sibling] {
            fs::write(p, b"#!/bin/sh\n").unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // 1. Sibling wins when present and executable.
        let found = resolve_controller_from(Some(&exe_path), &[&jb_bin, &mob_bin]);
        assert_eq!(found, Some(sibling.clone()));

        // 2. Non-executable sibling is skipped in favor of jb fallback.
        fs::set_permissions(&sibling, fs::Permissions::from_mode(0o644)).unwrap();
        let found = resolve_controller_from(Some(&exe_path), &[&jb_bin, &mob_bin]);
        assert_eq!(found, Some(jb_bin.clone()));

        // 3. When jb missing, mobile fallback wins.
        fs::remove_file(&jb_bin).unwrap();
        let found = resolve_controller_from(Some(&exe_path), &[&jb_bin, &mob_bin]);
        assert_eq!(found, Some(mob_bin.clone()));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn parses_controller_json_and_builds_cards() {
        let raw = r#"{
            "schema_version": 1,
            "command": "status",
            "status": "expiring_soon",
            "message": "Одна подпись скоро истечёт.",
            "config_exists": true,
            "apple_session_exists": true,
            "apple_session_valid": true,
            "daemon_installed": true,
            "daemon_enabled": true,
            "renew_before_hours": 48,
            "checked_at_unix": 1763810000,
            "apps": [
                {
                    "name": "SideStore",
                    "bundle_id": "com.SideStore.SideStore.TEAM123456",
                    "status": "ok",
                    "expires_at_unix": 1764413575,
                    "hours_remaining": 167,
                    "needs_renewal": false,
                    "renewed": false,
                    "message": "Действует ещё ~167 ч"
                },
                {
                    "name": "LiveContainer",
                    "bundle_id": "com.example.live",
                    "status": "expiring_soon",
                    "expires_at_unix": 1763876600,
                    "hours_remaining": 18,
                    "needs_renewal": true,
                    "renewed": false,
                    "message": "Скоро истечёт"
                },
                {
                    "name": "OldApp",
                    "bundle_id": "com.example.old",
                    "status": "expired",
                    "expires_at_unix": 1763802800,
                    "hours_remaining": 0,
                    "needs_renewal": true,
                    "renewed": false,
                    "message": "Истёк"
                }
            ],
            "last_error": null
        }"#;

        let report = parse_controller_json(raw).expect("valid JSON");
        let ui = build_ui_state(&report, &|_| 0);
        assert_eq!(ui.overall_tone, StatusTone::Amber);
        assert_eq!(ui.apps.len(), 3);

        assert_eq!(ui.apps[0].tone, StatusTone::Green);
        assert_eq!(ui.apps[0].badge_text, "Действует");
        assert_eq!(ui.apps[0].remaining_text, "Осталось: 6 дн. 23 ч.");

        assert_eq!(ui.apps[1].tone, StatusTone::Amber);
        assert_eq!(ui.apps[1].badge_text, "Скоро истечёт");
        assert_eq!(ui.apps[1].remaining_text, "Осталось: 18 ч. 30 мин.");

        assert_eq!(ui.apps[2].tone, StatusTone::Red);
        assert_eq!(ui.apps[2].badge_text, "Истёк");
        assert_eq!(ui.apps[2].remaining_text, "Истёк 2 ч. назад");

        assert_eq!(ui.readiness[0].value, "Активна");
        assert_eq!(ui.readiness[0].tone, StatusTone::Green);
        assert_eq!(ui.readiness[1].value, "Включена");
        assert_eq!(ui.readiness[1].tone, StatusTone::Green);
        assert_eq!(ui.readiness[2].value, "За 48 ч до истечения");
        assert!(ui.setup_hint.is_none());
    }

    #[test]
    fn builds_setup_hints_when_unconfigured() {
        let raw = r#"{
            "schema_version": 1,
            "command": "status",
            "status": "needs_setup",
            "message": "Требуется настройка.",
            "config_exists": false,
            "apple_session_exists": false,
            "apple_session_valid": false,
            "daemon_installed": false,
            "daemon_enabled": false,
            "renew_before_hours": 48,
            "checked_at_unix": 1763810000,
            "apps": [],
            "last_error": "config.plist not found"
        }"#;

        let report = parse_controller_json(raw).expect("valid JSON");
        let ui = build_ui_state(&report, &|_| 0);
        assert_eq!(ui.overall_tone, StatusTone::Amber);
        assert_eq!(ui.readiness[0].value, "Требуется вход");
        assert_eq!(ui.readiness[0].tone, StatusTone::Red);
        assert_eq!(ui.readiness[1].value, "Не установлена");
        assert_eq!(ui.readiness[1].tone, StatusTone::Amber);
        let hint = ui.setup_hint.expect("expected setup hint");
        assert!(hint.contains("config.plist"));
        assert!(hint.contains("Apple ID"));
        assert!(hint.contains("LaunchDaemon"));
    }

    #[test]
    fn formats_dates_accurately_across_leap_years_and_timezones() {
        // 2026-11-29 10:52:55 UTC = 1795949575
        // Wait: check 2024-02-29 23:30:00 UTC = 1709249400
        assert_eq!(unix_to_ymdhms(1_709_249_400, 0), (2024, 2, 29, 23, 30, 0));
        // With +03:00 offset -> 2024-03-01 02:30:00
        assert_eq!(unix_to_ymdhms(1_709_249_400, 10_800), (2024, 3, 1, 2, 30, 0));
        assert_eq!(format_date_ru(1_709_249_400, 0), "29 февр. 2024, 23:30");
        assert_eq!(format_hhmm(1_709_249_400, 10_800), "02:30");
    }

    #[test]
    fn executes_mock_controller_json_even_on_nonzero_exit() {
        let base = std::env::temp_dir().join(format!(
            "rebox-ui-exec-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let script = base.join("mock-controller");
        fs::write(
            &script,
            r#"#!/bin/sh
cat <<'EOF'
{"schema_version":1,"command":"refresh","status":"error","message":"Не удалось продлить.","config_exists":true,"apple_session_exists":true,"apple_session_valid":false,"daemon_installed":true,"daemon_enabled":false,"renew_before_hours":24,"checked_at_unix":1763810000,"apps":[{"name":"SideStore","bundle_id":"com.sidestore.app","status":"error","expires_at_unix":null,"hours_remaining":null,"needs_renewal":true,"renewed":false,"message":"Сессия истекла"}],"last_error":"Apple token expired"}
EOF
exit 1
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        let ui = execute_operation_with(Some(&script), Operation::Refresh, |_| 0);
        assert_eq!(ui.overall_tone, StatusTone::Red);
        assert_eq!(ui.apps.len(), 1);
        assert_eq!(ui.apps[0].tone, StatusTone::Red);
        assert_eq!(ui.apps[0].badge_text, "Ошибка");
        assert_eq!(ui.apps[0].detail_text.as_deref(), Some("Сессия истекла"));
        assert_eq!(ui.readiness[0].value, "Требуется вход");
        assert_eq!(ui.readiness[1].value, "Отключена");
        assert_eq!(ui.readiness[2].value, "За 24 ч до истечения");
        assert!(ui.diagnostics_body.contains("Apple token expired"));

        let missing = execute_operation_with(None, Operation::Status, |_| 0);
        assert_eq!(missing.overall_tone, StatusTone::Red);
        assert!(missing.setup_hint.is_some());

        let loading = UiState::initial_loading();
        assert_eq!(loading.readiness.len(), 3);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn formats_remaining_edge_cases() {
        assert_eq!(format_remaining(Some(1030), 1000, None), "Осталось менее 1 мин.");
        assert_eq!(format_remaining(Some(1600), 1000, None), "Осталось: 10 мин.");
        assert_eq!(format_remaining(Some(970), 1000, None), "Истёк только что");
        assert_eq!(format_remaining(Some(400), 1000, None), "Истёк 10 мин. назад");
        assert_eq!(format_remaining(None, 1000, Some(30)), "Осталось: ~1 дн. 6 ч.");
        assert_eq!(format_remaining(None, 1000, Some(0)), "Срок действия истёк");
        assert_eq!(format_remaining(None, 1000, None), "Срок неизвестен");
    }
}
