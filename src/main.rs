#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Desk Pager: a tray app that buzzes your phone (through ntfy) after 45 minutes of continuous sitting.

mod core;
mod secret;
mod sensors;

use std::{
    collections::VecDeque,
    ffi::OsStr,
    fs::OpenOptions,
    io::Write,
    os::windows::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use qrcode::{render::svg, QrCode};
use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder,
};
use windows::{
    core::{h, w, HSTRING},
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0},
        System::{
            SystemInformation::GetLocalTime,
            Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE},
        },
    },
};

const NTFY_SERVER: &str = "https://ntfy.sh";
const PAIR_LINK_PREFIX: &str = "deskpager://pair?topic=";
const PAIR_WINDOW: &str = "pair";
const QR_SIZE_PX: u32 = 240;
const POLL: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_ATTEMPTS: u32 = 3;
const SEND_RETRY_WAIT: Duration = Duration::from_secs(5);
const MAX_PENDING_SENDS: usize = 8;
const APP_DIR: &str = "DeskPager";
const SHORTCUT_NAME: &str = "Desk Pager.lnk";
const SYSTEM_DIR: &str = "System32";
const TASKKILL_EXE: &str = "taskkill.exe";
const CMD_EXE: &str = "cmd.exe";
const PING_EXE: &str = "ping.exe";
const POWERSHELL_EXE: &str = r"WindowsPowerShell\v1.0\powershell.exe";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DeskPager";
const LOG_FILE: &str = "desk-pager.log";
const SETTINGS_FILE: &str = "settings.cfg";
const TEST_BUZZ_ARG: &str = "--test-buzz";
const OPEN_PAIR_ARG: &str = "--open-pair";
const AUTOSTART_ARG: &str = "--autostart";
const INSTALL_ARG: &str = "--install";
const NO_INSTALL_ARG: &str = "--no-install";
const JUST_INSTALLED_ARG: &str = "--just-installed";
const UNINSTALL_ARG: &str = "--uninstall";
const BREAK_AFTER_SECS_ARG: &str = "--break-after-secs";
const MIN_BREAK_AFTER_SECS: u64 = 10;
const MAX_BREAK_AFTER_SECS: u64 = 5400;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskPager";

const MENU_PAIR: &str = "pair";
const MENU_TEST: &str = "test";
const MENU_MEETING: &str = "meeting";
const MENU_DESKTOP_NOTIFY: &str = "desktop_notify";
const MENU_PAUSE: &str = "pause";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";
const MEETING_HOLD_SECS: u64 = 90 * 60;
const AUMID_KEY: &str = r"Software\Classes\AppUserModelId\DeskPager";
const CLEAR_TITLE: &str = "deskpager-clear";

/// User-customizable two-tier break schedule (Quick Stretch + Long Break).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct ScheduleSettings {
    /// Quick stretch interval in minutes (`1..=180`, e.g. 20, 25, 30, 45, 60).
    short_every_mins: u64,
    /// Quick stretch pause duration in seconds (`5..=300`, e.g. 15, 20, 30, 45, 60).
    short_break_secs: u64,
    /// Cumulative sitting before a long break in minutes (`5..=360`, e.g. 60, 90, 120, 180).
    long_every_mins: u64,
    /// Long break duration in minutes required to reset the long session (`1..=60`, e.g. 5, 10, 15).
    long_break_mins: u64,
    /// Whether to show a silent Windows 11 notification alongside phone alerts (default `true`).
    desktop_notify: bool,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            short_every_mins: 30,
            short_break_secs: 30,
            long_every_mins: 120,
            long_break_mins: 10,
            desktop_notify: true,
        }
    }
}

impl ScheduleSettings {
    /// Clamps user-provided schedule values to safe ergonomic bounds while allowing custom numbers.
    fn sanitized(self) -> Self {
        let short_every_mins = self.short_every_mins.clamp(1, 180);
        let short_break_secs = self.short_break_secs.clamp(5, 300);
        let long_every_mins = self.long_every_mins.clamp(short_every_mins.max(5), 360);
        let long_break_mins = self.long_break_mins.clamp(1, 60);
        Self {
            short_every_mins,
            short_break_secs,
            long_every_mins,
            long_break_mins,
            desktop_notify: self.desktop_notify,
        }
    }

    /// Converts these schedule settings into a runtime `core::Config`.
    fn to_config(self, break_override: Option<u64>) -> core::Config {
        let s = self.sanitized();
        core::Config {
            break_after: break_override.unwrap_or(s.short_every_mins * 60),
            short_break_secs: s.short_break_secs,
            long_break_after: s.long_every_mins * 60,
            idle_away: s.long_break_mins * 60,
            ..core::Config::default()
        }
    }

    /// Formats settings as `key=value` lines for `%LOCALAPPDATA%\DeskPager\settings.cfg`.
    fn to_cfg_text(self) -> String {
        let s = self.sanitized();
        format!(
            "short_every_mins={}\nshort_break_secs={}\nlong_every_mins={}\nlong_break_mins={}\ndesktop_notify={}\n",
            s.short_every_mins,
            s.short_break_secs,
            s.long_every_mins,
            s.long_break_mins,
            if s.desktop_notify { 1 } else { 0 }
        )
    }

    /// Parses `key=value` lines, falling back to defaults for any missing field.
    fn from_cfg_text(raw: &str) -> Self {
        let mut s = Self::default();
        for line in raw.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v_trimmed = v.trim();
            match k.trim() {
                "short_every_mins" => {
                    if let Ok(val) = v_trimmed.parse::<u64>() {
                        s.short_every_mins = val;
                    }
                }
                "short_break_secs" => {
                    if let Ok(val) = v_trimmed.parse::<u64>() {
                        s.short_break_secs = val;
                    }
                }
                "long_every_mins" => {
                    if let Ok(val) = v_trimmed.parse::<u64>() {
                        s.long_every_mins = val;
                    }
                }
                "long_break_mins" => {
                    if let Ok(val) = v_trimmed.parse::<u64>() {
                        s.long_break_mins = val;
                    }
                }
                "desktop_notify" => {
                    s.desktop_notify = !matches!(v_trimmed, "0" | "false" | "off");
                }
                _ => {}
            }
        }
        s.sanitized()
    }
}

/// Shared between the tray menu, the pairing window and the timer thread.
struct AppState {
    topic: Mutex<String>,
    first_run: bool,
    just_installed: AtomicBool,
    quitting: AtomicBool,
    paused: AtomicBool,
    meeting_until: AtomicU64,
    outbox: Sender<Outgoing>,
    phone_alarm_active: AtomicBool,
    idle_secs: AtomicU64,
    sitting_secs: AtomicU64,
    long_sitting_secs: AtomicU64,
    stretch_pause_rem_secs: AtomicU64,
    stretches_completed: AtomicU64,
    reset_short_req: AtomicBool,
    reset_all_req: AtomicBool,
    break_override: Option<u64>,
    settings: Mutex<ScheduleSettings>,
    cfg: Mutex<core::Config>,
    call_app: Mutex<Option<String>>,
    pause_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    meeting_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
    desktop_notify_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
    autostart_item: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
}

/// What the pairing window shows.
#[derive(Serialize)]
struct PairingInfo {
    code: String,
    qr_svg: String,
    first_run: bool,
}

/// Live timer and sensor status for the desktop window.
#[derive(Serialize)]
struct StatusInfo {
    paused: bool,
    in_meeting: bool,
    autostart: bool,
    desktop_notify: bool,
    just_installed: bool,
    install_path: String,
    idle_secs: u64,
    sitting_secs: u64,
    sitting_minutes: u64,
    break_after_secs: u64,
    short_break_secs: u64,
    stretch_pause_rem_secs: u64,
    long_sitting_secs: u64,
    long_sitting_minutes: u64,
    long_break_after_secs: u64,
    long_break_mins: u64,
    stretches_completed: u64,
    call_app: Option<String>,
}

/// Builds a `PairingInfo` payload with SVG QR code from a topic string (including sanitized PC hostname).
fn build_pairing_info(topic: &str, first_run: bool) -> Result<PairingInfo, String> {
    let pc_name = std::env::var("COMPUTERNAME")
        .ok()
        .map(|s| {
            s.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(24)
                .collect::<String>()
        })
        .filter(|s| !s.is_empty());
    let link = match pc_name {
        Some(name) => format!("{PAIR_LINK_PREFIX}{topic}&name={name}"),
        None => format!("{PAIR_LINK_PREFIX}{topic}"),
    };
    let qr = QrCode::new(link.as_bytes()).map_err(|e| format!("QR code failed: {e}"))?;
    let qr_svg = qr.render::<svg::Color>().min_dimensions(QR_SIZE_PX, QR_SIZE_PX).build();
    Ok(PairingInfo { code: topic.to_string(), qr_svg, first_run })
}

/// Returns the pairing code and a QR code of the `deskpager://` pairing link.
#[tauri::command]
fn pairing_info(state: State<'_, AppState>) -> Result<PairingInfo, String> {
    let topic = state.topic.lock().map_err(|_| "topic lock poisoned")?.clone();
    build_pairing_info(&topic, state.first_run)
}

/// Rotates the 128-bit secret topic in Windows Credential Manager and returns the new QR and code.
#[tauri::command]
fn regenerate_pairing(state: State<'_, AppState>) -> Result<PairingInfo, String> {
    let new_topic = secret::regenerate().map_err(|e| format!("cannot regenerate topic: {e}"))?;
    if let Ok(mut g) = state.topic.lock() {
        *g = new_topic.clone();
    }
    log("pairing topic regenerated by user");
    build_pairing_info(&new_topic, false)
}

/// Dismisses the one-time "Installation Complete" banner in the dashboard UI.
#[tauri::command]
fn dismiss_install_banner(state: State<'_, AppState>) {
    state.just_installed.store(false, Ordering::Relaxed);
}

/// Returns live timer state for the desktop dashboard window.
#[tauri::command]
fn app_status(state: State<'_, AppState>) -> StatusInfo {
    let now = unix_secs();
    let sitting_secs = state.sitting_secs.load(Ordering::Relaxed);
    let long_sitting_secs = state.long_sitting_secs.load(Ordering::Relaxed);
    let cfg = state.cfg.lock().map(|g| *g).unwrap_or_default();
    let desktop_notify = state.settings.lock().map(|g| g.desktop_notify).unwrap_or(true);
    let call_app = state.call_app.lock().ok().and_then(|g| g.clone());
    let in_meeting = state.meeting_until.load(Ordering::Relaxed) > now;
    let install_path = canonical_install_exe()
        .or_else(|| std::env::current_exe().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    StatusInfo {
        paused: state.paused.load(Ordering::Relaxed),
        in_meeting,
        autostart: autostart_enabled(),
        desktop_notify,
        just_installed: state.just_installed.load(Ordering::Relaxed),
        install_path,
        idle_secs: state.idle_secs.load(Ordering::Relaxed),
        sitting_secs,
        sitting_minutes: sitting_secs / 60,
        break_after_secs: cfg.break_after,
        short_break_secs: cfg.short_break_secs,
        stretch_pause_rem_secs: state.stretch_pause_rem_secs.load(Ordering::Relaxed),
        long_sitting_secs,
        long_sitting_minutes: long_sitting_secs / 60,
        long_break_after_secs: cfg.long_break_after,
        long_break_mins: cfg.idle_away / 60,
        stretches_completed: state.stretches_completed.load(Ordering::Relaxed),
        call_app,
    }
}

/// Returns the user's current break schedule settings.
#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> ScheduleSettings {
    state.settings.lock().map(|g| *g).unwrap_or_default()
}

/// Validates, persists, and immediately applies updated two-tier break schedule settings.
#[tauri::command]
fn save_settings(
    state: State<'_, AppState>,
    short_every_mins: u64,
    short_break_secs: u64,
    long_every_mins: u64,
    long_break_mins: u64,
    desktop_notify: Option<bool>,
) -> Result<ScheduleSettings, String> {
    let prev_notify = state.settings.lock().map(|g| g.desktop_notify).unwrap_or(true);
    let updated = ScheduleSettings {
        short_every_mins,
        short_break_secs,
        long_every_mins,
        long_break_mins,
        desktop_notify: desktop_notify.unwrap_or(prev_notify),
    }
    .sanitized();
    persist_settings(updated)?;
    if let Ok(mut g) = state.settings.lock() {
        *g = updated;
    }
    if let Ok(mut g) = state.cfg.lock() {
        *g = updated.to_config(state.break_override);
    }
    if let Ok(guard) = state.desktop_notify_item.lock() {
        if let Some(item) = guard.as_ref() {
            report(item.set_checked(updated.desktop_notify), "desktop notify check");
        }
    }
    log(&format!(
        "schedule updated: short={}m/{}s, long={}m/{}m, desktop_notify={}",
        updated.short_every_mins,
        updated.short_break_secs,
        updated.long_every_mins,
        updated.long_break_mins,
        updated.desktop_notify
    ));
    Ok(updated)
}

/// Enables or disables silent Windows desktop notifications and syncs the tray menu checkmark.
#[tauri::command]
fn set_desktop_notify_cmd(state: State<'_, AppState>, enable: bool) -> Result<bool, String> {
    let mut updated = state.settings.lock().map(|g| *g).unwrap_or_default();
    updated.desktop_notify = enable;
    persist_settings(updated)?;
    if let Ok(mut g) = state.settings.lock() {
        *g = updated;
    }
    if let Ok(guard) = state.desktop_notify_item.lock() {
        if let Some(item) = guard.as_ref() {
            report(item.set_checked(enable), "desktop notify check");
        }
    }
    log(&format!("silent desktop notifications set to {enable}"));
    Ok(enable)
}

/// Resets the current quick-stretch counter while keeping cumulative long-break progress.
/// If clicked before the first stretch interval is reached, resets both so they never desync early.
#[tauri::command]
fn mark_quick_stretch(app: AppHandle, state: State<'_, AppState>) {
    let break_after = state.cfg.lock().map(|g| g.break_after).unwrap_or(1800);
    let long_secs = state.long_sitting_secs.load(Ordering::Relaxed);
    if long_secs >= break_after {
        state.reset_short_req.store(true, Ordering::Relaxed);
        state.sitting_secs.store(0, Ordering::Relaxed);
    } else {
        state.reset_all_req.store(true, Ordering::Relaxed);
        state.sitting_secs.store(0, Ordering::Relaxed);
        state.long_sitting_secs.store(0, Ordering::Relaxed);
        state.stretches_completed.store(0, Ordering::Relaxed);
    }
    if state.phone_alarm_active.swap(false, Ordering::Relaxed) {
        enqueue_buzz(&app, CLEAR_TITLE, "Break started at PC".into());
    }
    state.stretch_pause_rem_secs.store(0, Ordering::Relaxed);
}

/// Resets both the quick-stretch and 2-hour long-break timers to 0 immediately.
#[tauri::command]
fn reset_all_timers(app: AppHandle, state: State<'_, AppState>) {
    state.reset_all_req.store(true, Ordering::Relaxed);
    state.sitting_secs.store(0, Ordering::Relaxed);
    state.long_sitting_secs.store(0, Ordering::Relaxed);
    state.stretch_pause_rem_secs.store(0, Ordering::Relaxed);
    state.stretches_completed.store(0, Ordering::Relaxed);
    if state.phone_alarm_active.swap(false, Ordering::Relaxed) {
        enqueue_buzz(&app, CLEAR_TITLE, "Break started at PC".into());
    }
}

/// Toggles manual "In a meeting / presenting" hold mode (auto-expires after 90 min).
#[tauri::command]
fn toggle_meeting(state: State<'_, AppState>) -> bool {
    let now = unix_secs();
    let active = state.meeting_until.load(Ordering::Relaxed) > now;
    let next = !active;
    state.meeting_until.store(if next { now + MEETING_HOLD_SECS } else { 0 }, Ordering::Relaxed);
    if let Ok(guard) = state.meeting_item.lock() {
        if let Some(item) = guard.as_ref() {
            report(item.set_checked(next), "meeting check");
        }
    }
    next
}

/// Enables or disables "Start with Windows" from the dashboard UI and syncs the tray checkmark.
#[tauri::command]
fn set_autostart_cmd(state: State<'_, AppState>, enable: bool) -> Result<bool, String> {
    set_autostart(enable)?;
    let now_enabled = autostart_enabled();
    if let Ok(guard) = state.autostart_item.lock() {
        if let Some(item) = guard.as_ref() {
            report(item.set_checked(now_enabled), "autostart check");
        }
    }
    Ok(now_enabled)
}

/// Toggles the pause state from either the window or tray menu and syncs the tray label.
#[tauri::command]
fn toggle_pause(app: AppHandle, state: State<'_, AppState>) -> bool {
    let paused = !state.paused.fetch_xor(true, Ordering::Relaxed);
    if paused {
        state.sitting_secs.store(0, Ordering::Relaxed);
        state.long_sitting_secs.store(0, Ordering::Relaxed);
        if state.phone_alarm_active.swap(false, Ordering::Relaxed) {
            enqueue_buzz(&app, CLEAR_TITLE, "Break started at PC".into());
        }
    }
    if let Ok(guard) = state.pause_item.lock() {
        if let Some(item) = guard.as_ref() {
            report(item.set_text(if paused { "Resume" } else { "Pause" }), "pause label");
        }
    }
    paused
}

/// Sends a test buzz from the desktop window button (and silent PC notification if enabled).
#[tauri::command]
fn send_test_buzz(app: AppHandle) {
    maybe_notify_desktop(&app, "Desk Pager test", "Silent PC notification + phone test buzz active.");
    enqueue_buzz(&app, "Desk Pager test", "Test buzz from your laptop.".into());
}

/// Called by the pairing page once it has drawn itself, so a blank window shows up in the log.
#[tauri::command]
fn pairing_rendered(qr_shown: bool) {
    log(&format!("pairing window rendered, qr shown: {qr_shown}"));
}

/// Loads saved schedule settings from `%LOCALAPPDATA%\DeskPager\settings.cfg`, or defaults.
fn load_settings() -> ScheduleSettings {
    std::env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join(APP_DIR).join(SETTINGS_FILE))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|raw| ScheduleSettings::from_cfg_text(&raw))
        .unwrap_or_default()
}

/// Writes schedule settings to `%LOCALAPPDATA%\DeskPager\settings.cfg` (never contains the secret topic).
fn persist_settings(settings: ScheduleSettings) -> Result<(), String> {
    let base = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA not set")?;
    let dir = PathBuf::from(base).join(APP_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(SETTINGS_FILE), settings.to_cfg_text()).map_err(|e| e.to_string())
}

/// Parses `--break-after-secs <10..=5400>` from command-line arguments.
///
/// * `args` - Full argument slice including binary name.
fn parse_break_after_secs(args: &[String]) -> Result<Option<u64>, String> {
    let Some(pos) = args.iter().position(|a| a == BREAK_AFTER_SECS_ARG) else {
        return Ok(None);
    };
    let Some(raw) = args.get(pos + 1) else {
        return Err(format!("{BREAK_AFTER_SECS_ARG} requires a value in {MIN_BREAK_AFTER_SECS}..={MAX_BREAK_AFTER_SECS}"));
    };
    let secs: u64 = raw
        .parse()
        .map_err(|_| format!("invalid {BREAK_AFTER_SECS_ARG} value '{raw}': expected integer seconds"))?;
    if !(MIN_BREAK_AFTER_SECS..=MAX_BREAK_AFTER_SECS).contains(&secs) {
        return Err(format!(
            "{BREAK_AFTER_SECS_ARG} {secs} out of range ({MIN_BREAK_AFTER_SECS}..={MAX_BREAK_AFTER_SECS})"
        ));
    }
    Ok(Some(secs))
}

fn main() {
    // With panic = "abort" and no console, a panic (Tauri panics on a setup error) would otherwise leave no trace.
    std::panic::set_hook(Box::new(|info| log(&format!("panic: {info}"))));
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == UNINSTALL_ARG) {
        uninstall_desk_pager();
        return;
    }
    let action = plan_install(&args);
    if action != InstallAction::RunInPlace {
        match self_install_and_relaunch(&args, action) {
            Ok(()) => return,
            Err(e) => log(&format!("self-install failed (running in place): {e}")),
        }
    }
    let break_override = match parse_break_after_secs(&args) {
        Ok(v) => v,
        Err(e) => {
            log(&format!("argument error: {e}"));
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if !claim_single_instance() {
        return;
    }
    let topic = match secret::load_or_create() {
        Ok(topic) => topic,
        Err(e) => {
            log(&format!("cannot read or create the pairing secret: {e}"));
            std::process::exit(1);
        }
    };
    ensure_aumid_registered();
    sensors::spawn();

    let first_run = topic.is_new;
    let just_installed = args.iter().any(|a| a == JUST_INSTALLED_ARG);
    let is_autostart = args.iter().any(|a| a == AUTOSTART_ARG);
    let settings = load_settings();
    let cfg = settings.to_config(break_override);
    let (outbox, outbox_rx) = mpsc::channel();
    let built = tauri::Builder::default()
        .manage(AppState {
            topic: Mutex::new(topic.value),
            first_run,
            just_installed: AtomicBool::new(just_installed),
            quitting: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            meeting_until: AtomicU64::new(0),
            outbox,
            phone_alarm_active: AtomicBool::new(false),
            idle_secs: AtomicU64::new(0),
            sitting_secs: AtomicU64::new(0),
            long_sitting_secs: AtomicU64::new(0),
            stretch_pause_rem_secs: AtomicU64::new(0),
            stretches_completed: AtomicU64::new(0),
            reset_short_req: AtomicBool::new(false),
            reset_all_req: AtomicBool::new(false),
            break_override,
            settings: Mutex::new(settings),
            cfg: Mutex::new(cfg),
            call_app: Mutex::new(None),
            pause_item: Mutex::new(None),
            meeting_item: Mutex::new(None),
            desktop_notify_item: Mutex::new(None),
            autostart_item: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            pairing_info,
            regenerate_pairing,
            dismiss_install_banner,
            pairing_rendered,
            app_status,
            get_settings,
            save_settings,
            set_desktop_notify_cmd,
            mark_quick_stretch,
            reset_all_timers,
            toggle_meeting,
            set_autostart_cmd,
            toggle_pause,
            send_test_buzz
        ])
        .setup(move |app| {
            let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
            let pair = MenuItem::with_id(app, MENU_PAIR, "Open Desk Pager…", true, None::<&str>)?;
            let test = MenuItem::with_id(app, MENU_TEST, "Send test buzz", true, None::<&str>)?;
            let meeting = CheckMenuItem::with_id(
                app, MENU_MEETING, "In a meeting / presenting", true, false, None::<&str>,
            )?;
            let desktop_notify = CheckMenuItem::with_id(
                app,
                MENU_DESKTOP_NOTIFY,
                "Silent PC notifications",
                true,
                settings.desktop_notify,
                None::<&str>,
            )?;
            let pause = MenuItem::with_id(app, MENU_PAUSE, "Pause", true, None::<&str>)?;
            let autostart = CheckMenuItem::with_id(
                app, MENU_AUTOSTART, "Start with Windows", true, autostart_enabled(), None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[
                    &status,
                    &separator,
                    &pair,
                    &test,
                    &meeting,
                    &desktop_notify,
                    &pause,
                    &autostart,
                    &separator,
                    &quit,
                ],
            )?;

            {
                let state = app.state::<AppState>();
                if let Ok(mut g) = state.pause_item.lock() {
                    *g = Some(pause.clone());
                }
                if let Ok(mut g) = state.meeting_item.lock() {
                    *g = Some(meeting.clone());
                }
                if let Ok(mut g) = state.desktop_notify_item.lock() {
                    *g = Some(desktop_notify.clone());
                }
                if let Ok(mut g) = state.autostart_item.lock() {
                    *g = Some(autostart.clone());
                };
            }
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().ok_or("bundle icon missing")?)
                .tooltip("Desk Pager")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        open_pairing(tray.app_handle());
                    }
                })
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    MENU_PAIR => open_pairing(app),
                    MENU_TEST => {
                        maybe_notify_desktop(app, "Desk Pager test", "Silent PC notification + phone test buzz active.");
                        enqueue_buzz(app, "Desk Pager test", "Test buzz from your laptop.".into());
                    }
                    MENU_MEETING => {
                        let _ = toggle_meeting(app.state::<AppState>());
                    }
                    MENU_DESKTOP_NOTIFY => {
                        let state = app.state::<AppState>();
                        let current = state.settings.lock().map(|g| g.desktop_notify).unwrap_or(true);
                        let _ = set_desktop_notify_cmd(state, !current);
                    }
                    MENU_PAUSE => {
                        let _ = toggle_pause(app.clone(), app.state::<AppState>());
                    }
                    MENU_AUTOSTART => {
                        let enable = !autostart_enabled();
                        if let Err(e) = set_autostart_cmd(app.state::<AppState>(), enable) {
                            log(&format!("start with Windows change failed: {e}"));
                        }
                    }
                    MENU_QUIT => {
                        app.state::<AppState>().quitting.store(true, Ordering::SeqCst);
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            let handle = app.handle().clone();
            std::thread::spawn(move || run_timer(&handle, &status, &tray));
            let sender_handle = app.handle().clone();
            std::thread::spawn(move || run_sender(outbox_rx, |msg| deliver(&sender_handle, msg)));
            spawn_show_window_listener(app.handle().clone());

            if just_installed {
                let _ = show_silent_desktop_notification(
                    "Desk Pager is ready",
                    "Installed to your Start Menu and running quietly in the system tray.",
                );
            }
            if !is_autostart || first_run || just_installed || args.iter().any(|arg| arg == OPEN_PAIR_ARG) {
                open_pairing(app.handle());
            }
            if args.iter().any(|arg| arg == TEST_BUZZ_ARG) {
                maybe_notify_desktop(
                    app.handle(),
                    "Desk Pager test",
                    "Silent PC notification + phone test buzz active.",
                );
                enqueue_buzz(app.handle(), "Desk Pager test", "Test buzz from your laptop.".into());
            }
            log(&format!(
                "started, first run: {first_run}, just_installed: {just_installed}, break_after: {}s, long_break_after: {}s, desktop_notify: {}",
                cfg.break_after, cfg.long_break_after, settings.desktop_notify
            ));
            Ok(())
        })
        .build(tauri::generate_context!());
    let app = match built {
        Ok(app) => app,
        Err(e) => {
            log(&format!("failed to build tauri app: {e}"));
            std::process::exit(1);
        }
    };

    app.run(|app_handle, event| {
        // Closing the dashboard destroys it (freeing WebView2); the tray app lives on until MENU_QUIT.
        if let RunEvent::ExitRequested { api, .. } = event {
            if !app_handle.state::<AppState>().quitting.load(Ordering::SeqCst) {
                api.prevent_exit();
            }
        }
    });
}

/// Samples the sensors every few seconds, drives the sitting timer and keeps the tray text current.
fn run_timer(app: &AppHandle, status: &MenuItem<tauri::Wry>, tray: &TrayIcon) {
    let mut tracker = core::Tracker::default();
    let mut suspends_seen = sensors::suspend_count();
    let mut shown = String::new();
    let mut first_tick = true;
    let mut capture_failing = false;
    loop {
        let now = unix_secs();
        let state = app.state::<AppState>();
        let cfg = state.cfg.lock().map(|g| *g).unwrap_or_default();
        if state.reset_all_req.swap(false, Ordering::Relaxed) {
            tracker.reset();
        }
        if state.reset_short_req.swap(false, Ordering::Relaxed) {
            tracker.reset_short();
        }
        let text = if state.paused.load(Ordering::Relaxed) {
            tracker.reset();
            state.sitting_secs.store(0, Ordering::Relaxed);
            state.long_sitting_secs.store(0, Ordering::Relaxed);
            if let Ok(mut guard) = state.call_app.lock() {
                *guard = None;
            }
            "Paused".to_string()
        } else {
            let suspends = sensors::suspend_count();
            if suspends != suspends_seen {
                suspends_seen = suspends;
                tracker.reset();
                state.meeting_until.store(0, Ordering::Relaxed);
                if state.phone_alarm_active.swap(false, Ordering::Relaxed) {
                    enqueue_buzz(app, CLEAR_TITLE, "Break started at PC".into());
                }
            }
            let locked = sensors::is_locked();
            if locked {
                state.meeting_until.store(0, Ordering::Relaxed);
                if state.phone_alarm_active.swap(false, Ordering::Relaxed) {
                    enqueue_buzz(app, CLEAR_TITLE, "Break started at PC".into());
                }
            }
            match sensors::idle_secs() {
                Ok(idle) => {
                    state.idle_secs.store(idle, Ordering::Relaxed);
                    if idle >= cfg.short_break_secs.min(30)
                        && state.phone_alarm_active.swap(false, Ordering::Relaxed)
                    {
                        enqueue_buzz(app, CLEAR_TITLE, "Break started at PC".into());
                    }
                    let scan = sensors::capture_apps();
                    let capture_error = scan.first_error.as_ref().map(ToString::to_string);
                    if let Some(line) = capture_transition(capture_failing, capture_error.as_deref()) {
                        log(&line);
                    }
                    capture_failing = capture_error.is_some();
                    let mic_app = core::active_call_app(&scan.apps).map(str::to_string);
                    let in_meeting = state.meeting_until.load(Ordering::Relaxed) > now;
                    let hold_label = mic_app.clone().or_else(|| in_meeting.then(|| "Meeting mode".to_string()));
                    if let Ok(mut guard) = state.call_app.lock() {
                        *guard = hold_label.clone();
                    }
                    if let Some(alert) = tracker.tick(&cfg, now, idle, locked, hold_label.is_some()) {
                        match alert.kind {
                            core::AlertKind::QuickStretch => {
                                let body = core::stretch_prompt(
                                    tracker.stretches_completed(),
                                    alert.minutes,
                                    cfg.short_break_secs,
                                );
                                maybe_notify_desktop(app, "Quick stretch & eye rest", &body);
                                enqueue_buzz(app, "Quick stretch & eye rest", body);
                            }
                            core::AlertKind::LongBreak => {
                                let body = format!(
                                    "At your desk for {} min ({} stretches done) — step away for {} min to walk, hydrate, and unload your lower back.",
                                    alert.minutes,
                                    tracker.stretches_completed(),
                                    cfg.idle_away / 60
                                );
                                state.phone_alarm_active.store(true, Ordering::Relaxed);
                                maybe_notify_desktop(app, "Time for a long break", &body);
                                enqueue_buzz(app, "Time for a long break", body);
                            }
                        }
                    }
                    state.sitting_secs.store(tracker.seconds(now), Ordering::Relaxed);
                    state.long_sitting_secs.store(tracker.long_seconds(now), Ordering::Relaxed);
                    state
                        .stretch_pause_rem_secs
                        .store(tracker.stretch_pause_remaining(&cfg, now), Ordering::Relaxed);
                    state
                        .stretches_completed
                        .store(u64::from(tracker.stretches_completed()), Ordering::Relaxed);
                    let short_target = (cfg.break_after / 60).max(1);
                    let short_min = tracker.minutes(now).min(short_target);
                    let long_target = (cfg.long_break_after / 60).max(1);
                    let long_min = tracker.long_minutes(now).min(long_target);
                    match hold_label {
                        Some(name) => format!("Stretch {short_min}/{short_target}m · Long {long_min}/{long_target}m ({name})"),
                        None => format!("Stretch {short_min}/{short_target}m · Long {long_min}/{long_target}m"),
                    }
                }
                Err(e) => {
                    log(&format!("idle time unavailable: {e}"));
                    "Can't read activity".to_string()
                }
            }
        };
        if text != shown {
            report(status.set_text(&text), "status text");
            if !first_tick {
                report(tray.set_tooltip(Some(format!("Desk Pager — {text}"))), "tooltip");
            }
            shown = text;
        }
        first_tick = false;
        std::thread::sleep(POLL);
    }
}

/// Returns the line to log when call detection starts failing or recovers, `None` while its state is unchanged.
///
/// * `was_failing` - Whether the previous scan reported an error.
/// * `error` - This scan's error text, if any.
fn capture_transition(was_failing: bool, error: Option<&str>) -> Option<String> {
    match (was_failing, error) {
        (false, Some(e)) => Some(format!("call detection failing (some microphone sessions unreadable): {e}")),
        (true, None) => Some("call detection recovered".to_string()),
        _ => None,
    }
}

/// Registers `DeskPager` in `HKCU\Software\Classes\AppUserModelId\DeskPager` so Windows 11 Notification Center accepts silent toast notifications without an MSIX installer.
fn ensure_aumid_registered() {
    if let Ok(key) = windows_registry::CURRENT_USER.create(AUMID_KEY) {
        let _ = key.set_string("DisplayName", "Desk Pager");
    }
}

/// Escapes XML special characters for Windows Toast XML templates.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Posts a silent native Windows 11 notification if the user has `desktop_notify` enabled.
fn maybe_notify_desktop(app: &AppHandle, title: &str, body: &str) {
    let enabled = app
        .state::<AppState>()
        .settings
        .lock()
        .map(|g| g.desktop_notify)
        .unwrap_or(true);
    if !enabled {
        return;
    }
    if let Err(e) = show_silent_desktop_notification(title, body) {
        log(&format!("desktop notification failed: {e}"));
    }
}

/// Displays a silent (`<audio silent="true"/>`), non-focus-stealing notification in the Windows 11 Notification Center.
fn show_silent_desktop_notification(title: &str, body: &str) -> Result<(), String> {
    let xml_str = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual><audio silent=\"true\"/></toast>",
        escape_xml(title),
        escape_xml(body)
    );
    let doc = XmlDocument::new().map_err(|e| e.to_string())?;
    doc.LoadXml(&HSTRING::from(xml_str)).map_err(|e| e.to_string())?;
    let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.to_string())?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(h!("DeskPager"))
        .map_err(|e| e.to_string())?;
    notifier.Show(&toast).map_err(|e| e.to_string())
}

/// One phone message waiting for the sender thread.
#[derive(Debug, PartialEq, Eq)]
struct Outgoing {
    title: &'static str,
    body: String,
}

/// Pending phone messages in send order, capped so a long outage cannot pile up stale buzzes.
#[derive(Default)]
struct SendQueue {
    pending: VecDeque<Outgoing>,
}

impl SendQueue {
    /// Appends `msg`; past `MAX_PENDING_SENDS` it returns the oldest pending non-clear message, dropped to make room.
    fn push(&mut self, msg: Outgoing) -> Option<Outgoing> {
        self.pending.push_back(msg);
        if self.pending.len() <= MAX_PENDING_SENDS {
            return None;
        }
        // A clear is never dropped: losing one can leave the phone alarm ringing.
        let oldest_buzz = self.pending.iter().position(|m| m.title != CLEAR_TITLE)?;
        self.pending.remove(oldest_buzz)
    }

    /// Takes the next message to send, oldest first.
    fn pop(&mut self) -> Option<Outgoing> {
        self.pending.pop_front()
    }
}

/// Body of the one sender thread: hands messages to `send` in order until every `Sender` is gone.
///
/// * `rx` - Receiving end of the channel held in `AppState::outbox`.
/// * `send` - Blocking delivery of one message.
fn run_sender(rx: Receiver<Outgoing>, mut send: impl FnMut(Outgoing)) {
    let mut queue = SendQueue::default();
    loop {
        let next = match queue.pop() {
            Some(msg) => msg,
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => return,
            },
        };
        send(next);
        while let Ok(msg) = rx.try_recv() {
            if let Some(dropped) = queue.push(msg) {
                log(&format!("buzz dropped (more than {MAX_PENDING_SENDS} waiting): {}", dropped.title));
            }
        }
    }
}

/// Queues one buzz for the sender thread and returns at once, so the caller never waits on the network.
fn enqueue_buzz(app: &AppHandle, title: &'static str, body: String) {
    if app.state::<AppState>().outbox.send(Outgoing { title, body }).is_err() {
        log(&format!("buzz not queued (sender thread stopped): {title}"));
    }
}

/// Sends one buzz to the phone, retrying a few times, and logs the outcome (never the topic).
fn deliver(app: &AppHandle, msg: Outgoing) {
    let Outgoing { title, body } = msg;
    let topic = match app.state::<AppState>().topic.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return log(&format!("buzz not sent (topic lock poisoned): {title}")),
    };
    for attempt in 1..=SEND_ATTEMPTS {
        match tauri::async_runtime::block_on(send(&topic, title, body.clone())) {
            Ok(()) => return log(&format!("buzz sent: {title}")),
            Err(e) => log(&format!("buzz failed (attempt {attempt}/{SEND_ATTEMPTS}): {e}")),
        }
        if attempt < SEND_ATTEMPTS {
            std::thread::sleep(SEND_RETRY_WAIT);
        }
    }
}

/// POSTs one high-priority message to the ntfy topic.
async fn send(topic: &str, title: &str, body: String) -> Result<(), String> {
    // without_url(): the URL contains the topic, which is the pairing secret.
    let strip = |e: reqwest::Error| e.without_url().to_string();
    let client = reqwest::Client::builder().timeout(SEND_TIMEOUT).build().map_err(strip)?;
    client
        .post(format!("{NTFY_SERVER}/{topic}"))
        .header("Title", title)
        .header("Priority", "high")
        .header("Tags", "chair")
        .body(body)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map(|_| ())
        .map_err(strip)
}

/// Opens (or unminimizes, shows, and focuses) the dashboard window.
fn open_pairing(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(PAIR_WINDOW) {
        report(window.unminimize(), "pairing unminimize");
        report(window.show(), "pairing show");
        report(window.set_focus(), "pairing focus");
        return;
    }
    let app = app.clone();
    // Building a webview from a main-thread event handler can deadlock on Windows; build off-thread.
    tauri::async_runtime::spawn(async move {
        let built = WebviewWindowBuilder::new(&app, PAIR_WINDOW, WebviewUrl::App("index.html".into()))
            .title("Desk Pager")
            .inner_size(940.0, 650.0)
            .min_inner_size(460.0, 580.0)
            .resizable(true)
            .build();
        if let Err(e) = built {
            log(&format!("pairing window failed: {e}"));
        }
    });
}

/// Returns the canonical per-user installation path (`%LOCALAPPDATA%\Programs\DeskPager\desk-pager.exe`).
fn canonical_install_exe() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|dir| PathBuf::from(dir).join("Programs").join(APP_DIR).join("desk-pager.exe"))
}

/// What a launch from outside the install folder should do about installation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallAction {
    /// Leave the installation alone and run this executable where it is.
    RunInPlace,
    /// The installed copy is this same binary: start (or show) it and exit.
    ShowInstalled,
    /// Replace the installed binary, keeping the user's autostart and shortcut choices.
    Upgrade,
    /// Copy the binary, create shortcuts and enable Start with Windows.
    FirstInstall,
}

/// Decides the install action from already-gathered facts.
///
/// * `installed_exists` - An exe already sits at the canonical install path.
/// * `identical` - That exe is byte-identical to the running one.
/// * `explicit` - `--install` was passed.
/// * `from_cargo_target` - Running from a Cargo `target\debug` or `target\release` tree.
fn install_action(installed_exists: bool, identical: bool, explicit: bool, from_cargo_target: bool) -> InstallAction {
    if explicit {
        InstallAction::FirstInstall
    } else if from_cargo_target {
        InstallAction::RunInPlace
    } else if !installed_exists {
        InstallAction::FirstInstall
    } else if identical {
        InstallAction::ShowInstalled
    } else {
        InstallAction::Upgrade
    }
}

/// Gathers the facts for `install_action` from the arguments, the running exe and the installed copy.
fn plan_install(args: &[String]) -> InstallAction {
    if args.iter().any(|a| a == NO_INSTALL_ARG || a == JUST_INSTALLED_ARG || a == UNINSTALL_ARG) {
        return InstallAction::RunInPlace;
    }
    let (Some(target), Ok(current)) = (canonical_install_exe(), std::env::current_exe()) else {
        return InstallAction::RunInPlace;
    };
    let cur_norm = current.to_string_lossy().to_ascii_lowercase();
    if cur_norm == target.to_string_lossy().to_ascii_lowercase() {
        return InstallAction::RunInPlace;
    }
    let installed_exists = target.exists();
    let identical = installed_exists
        && files_identical(&current, &target).unwrap_or_else(|e| {
            log(&format!("cannot compare with the installed copy (treating it as different): {e}"));
            false
        });
    let explicit = args.iter().any(|a| a == INSTALL_ARG);
    let from_cargo_target = cur_norm.contains("\\target\\debug\\") || cur_norm.contains("\\target\\release\\");
    install_action(installed_exists, identical, explicit, from_cargo_target)
}

/// Whether two files hold the same bytes (length is compared first, contents only when it matches).
fn files_identical(a: &Path, b: &Path) -> std::io::Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    Ok(std::fs::read(a)? == std::fs::read(b)?)
}

/// Returns `%SystemRoot%\System32`, so helper programs are never resolved by bare name.
fn system_dir() -> Result<PathBuf, String> {
    system_dir_from(std::env::var_os("SystemRoot").as_deref())
}

/// Builds the system directory from a `SystemRoot` value, refusing a missing or relative one.
fn system_dir_from(system_root: Option<&OsStr>) -> Result<PathBuf, String> {
    let root = Path::new(system_root.ok_or("SystemRoot is not set")?);
    if !root.is_absolute() {
        return Err("SystemRoot is not an absolute path".to_string());
    }
    Ok(root.join(SYSTEM_DIR))
}

/// Logs a failed install or uninstall step and remembers its name for the final summary line.
fn record_step(failed: &mut Vec<&'static str>, name: &'static str, result: Result<(), String>) {
    if let Err(e) = result {
        log(&format!("step failed: {name}: {e}"));
        failed.push(name);
    }
}

/// Force-stops every other running `desk-pager.exe` so its binary can be replaced or removed.
fn kill_other_instances(system_dir: &Path) -> Result<(), String> {
    let my_pid = std::process::id();
    // taskkill's exit code is ignored: it is non-zero whenever no other copy was running.
    Command::new(system_dir.join(TASKKILL_EXE))
        .args(["/F", "/IM", "desk-pager.exe", "/FI", &format!("PID ne {my_pid}")])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|_| ())
        .map_err(|e| format!("cannot run taskkill: {e}"))
}

/// Carries out `action`: starts the identical installed copy, or copies this binary to
/// `%LOCALAPPDATA%\Programs\DeskPager\desk-pager.exe` and relaunches it. Only a first install creates
/// shortcuts and enables Start with Windows; an upgrade leaves both as the user set them.
fn self_install_and_relaunch(args: &[String], action: InstallAction) -> Result<(), String> {
    let target = canonical_install_exe().ok_or("LOCALAPPDATA not set")?;
    let install_dir = target.parent().ok_or("invalid install directory")?;
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut launch_args: Vec<String> = args
        .iter()
        .skip(1)
        .filter(|a| *a != INSTALL_ARG && *a != JUST_INSTALLED_ARG && *a != OPEN_PAIR_ARG)
        .cloned()
        .collect();
    launch_args.push(OPEN_PAIR_ARG.to_string());

    if action == InstallAction::ShowInstalled {
        Command::new(&target)
            .args(&launch_args)
            .spawn()
            .map_err(|e| format!("starting the installed copy failed: {e}"))?;
        log("installed copy is identical to this one; started it without reinstalling");
        return Ok(());
    }

    let first_install = action == InstallAction::FirstInstall;
    let system_dir = system_dir()?;
    let mut failed = Vec::new();

    record_step(&mut failed, "stop running copy", kill_other_instances(&system_dir));
    std::thread::sleep(Duration::from_millis(350));

    std::fs::create_dir_all(install_dir).map_err(|e| format!("create install dir failed: {e}"))?;
    std::fs::copy(&current, &target).map_err(|e| format!("copy binary failed: {e}"))?;

    record_step(&mut failed, "uninstall registry entry", register_uninstaller(&target, install_dir));
    if first_install {
        for (name, path) in shortcut_paths() {
            let created = path.and_then(|lnk| create_windows_shortcut(&system_dir, &target, &lnk));
            record_step(&mut failed, name, created);
        }
        record_step(&mut failed, "start with Windows", set_autostart(true));
        launch_args.push(JUST_INSTALLED_ARG.to_string());
    }

    let verb = if first_install { "installed" } else { "upgraded" };
    let outcome = if failed.is_empty() {
        "all steps ok".to_string()
    } else {
        format!("failed steps: {}", failed.join(", "))
    };
    Command::new(&target)
        .args(&launch_args)
        .spawn()
        .map_err(|e| format!("{verb} to {} ({outcome}) but relaunching it failed: {e}", target.display()))?;
    log(&format!("{verb} Desk Pager to {} ({outcome})", target.display()));
    Ok(())
}

/// Registers Desk Pager under `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskPager`.
fn register_uninstaller(target_exe: &Path, install_dir: &Path) -> Result<(), String> {
    let exe_str = target_exe.display().to_string();
    let write = || -> windows_registry::Result<()> {
        let key = windows_registry::CURRENT_USER.create(UNINSTALL_KEY)?;
        key.set_string("DisplayName", "Desk Pager")?;
        key.set_string("DisplayVersion", env!("CARGO_PKG_VERSION"))?;
        key.set_string("Publisher", "Desk Pager")?;
        key.set_string("DisplayIcon", &exe_str)?;
        key.set_string("InstallLocation", install_dir.display().to_string())?;
        key.set_string("UninstallString", format!("\"{exe_str}\" {UNINSTALL_ARG}"))?;
        key.set_u32("NoModify", 1)?;
        key.set_u32("NoRepair", 1)
    };
    write().map_err(|e| e.to_string())
}

/// Start Menu and Desktop shortcut locations, each `Err` when its base folder variable is unset.
fn shortcut_paths() -> [(&'static str, Result<PathBuf, String>); 2] {
    let base = |var: &str| std::env::var_os(var).map(PathBuf::from).ok_or(format!("{var} not set"));
    [
        (
            "Start Menu shortcut",
            base("APPDATA").map(|dir| {
                dir.join("Microsoft").join("Windows").join("Start Menu").join("Programs").join(SHORTCUT_NAME)
            }),
        ),
        ("Desktop shortcut", base("USERPROFILE").map(|dir| dir.join("Desktop").join(SHORTCUT_NAME))),
    ]
}

/// Uses `WScript.Shell` via PowerShell to write a standard Windows `.lnk` shortcut without extra crates.
fn create_windows_shortcut(system_dir: &Path, target_exe: &Path, shortcut_path: &Path) -> Result<(), String> {
    let tgt = target_exe.display().to_string().replace('\'', "''");
    let lnk = shortcut_path.display().to_string().replace('\'', "''");
    let script = format!(
        "$ws = New-Object -ComObject WScript.Shell; $s = $ws.CreateShortcut('{lnk}'); $s.TargetPath = '{tgt}'; $s.Arguments = '{OPEN_PAIR_ARG}'; $s.Description = 'Desk Pager — Posture, Eye Rest & Hydration Reminders'; $s.IconLocation = '{tgt},0'; $s.Save()"
    );
    let status = Command::new(system_dir.join(POWERSHELL_EXE))
        .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("cannot run powershell: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("powershell ended with {status}"))
    }
}

/// Maps a "not found" I/O error to success, for removing things that may already be gone.
fn ignore_missing(result: std::io::Result<()>) -> Result<(), String> {
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// Removes a registry tree under HKCU; a key that is already absent counts as success.
fn remove_reg_tree(path: &str) -> Result<(), String> {
    match windows_registry::CURRENT_USER.remove_tree(path) {
        Err(e) if windows_registry::CURRENT_USER.open(path).is_ok() => Err(e.to_string()),
        _ => Ok(()),
    }
}

/// Returns `dir` as text when it is exactly the canonical install directory and safe to quote for `cmd.exe`.
///
/// * `dir` - Directory about to be removed.
/// * `local_app_data` - Value of `%LOCALAPPDATA%`.
fn checked_install_dir(dir: &Path, local_app_data: &Path) -> Result<String, String> {
    if !local_app_data.is_absolute() || local_app_data.components().any(|c| c == Component::ParentDir) {
        return Err("LOCALAPPDATA is not a plain absolute path".to_string());
    }
    if dir != local_app_data.join("Programs").join(APP_DIR) {
        return Err("not the Desk Pager install directory".to_string());
    }
    let text = dir.to_str().ok_or("install directory is not valid Unicode")?;
    // cmd.exe still expands %VAR% inside double quotes, and a quote would end the quoting.
    if text.contains(['"', '%']) {
        return Err("install directory contains a character cmd.exe would reinterpret".to_string());
    }
    Ok(text.to_string())
}

/// Builds the raw `cmd.exe` arguments that wait about a second (so this exe can exit) and then remove `dir`.
///
/// * `ping` - Absolute path of `ping.exe`, used as the delay.
/// * `dir` - Directory to remove; must not contain `"` or `%`.
fn rmdir_command_line(ping: &Path, dir: &str) -> String {
    // cmd strips the outermost quote pair after /C, leaving each inner path correctly quoted.
    format!("/C \"\"{}\" 127.0.0.1 -n 2 > nul & rmdir /S /Q \"{dir}\"\"", ping.display())
}

/// Starts a detached `cmd.exe` that removes the install folder once this process has exited.
fn schedule_install_dir_removal(system_dir: &Path) -> Result<(), String> {
    let local_app_data = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA not set")?);
    let exe = canonical_install_exe().ok_or("LOCALAPPDATA not set")?;
    let dir = checked_install_dir(exe.parent().ok_or("invalid install directory")?, &local_app_data)?;
    Command::new(system_dir.join(CMD_EXE))
        .raw_arg(rmdir_command_line(&system_dir.join(PING_EXE), &dir))
        // Start outside the install folder: a process whose working directory is inside it blocks rmdir.
        .current_dir(system_dir)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot run cmd: {e}"))
}

/// Removes `%LOCALAPPDATA%\DeskPager` (log and settings).
fn remove_data_dir() -> Result<(), String> {
    let local_app_data = PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA not set")?);
    if !local_app_data.is_absolute() {
        return Err("LOCALAPPDATA is not an absolute path".to_string());
    }
    ignore_missing(std::fs::remove_dir_all(local_app_data.join(APP_DIR)))
}

/// Uninstalls Desk Pager when invoked from Windows Settings -> Installed Apps (`--uninstall`): stops the running
/// copy, then removes autostart, registry entries, shortcuts, the pairing secret, the install folder and app data.
fn uninstall_desk_pager() {
    let mut failed = Vec::new();
    let system_dir = system_dir();
    record_step(&mut failed, "stop running copy", system_dir.clone().and_then(|dir| kill_other_instances(&dir)));
    record_step(&mut failed, "start with Windows entry", set_autostart(false));
    record_step(&mut failed, "uninstall registry entry", remove_reg_tree(UNINSTALL_KEY));
    record_step(&mut failed, "notification registry entry", remove_reg_tree(AUMID_KEY));
    for (name, path) in shortcut_paths() {
        record_step(&mut failed, name, path.and_then(|lnk| ignore_missing(std::fs::remove_file(lnk))));
    }
    record_step(&mut failed, "pairing secret", secret::delete().map_err(|e| e.to_string()));
    record_step(&mut failed, "install folder", system_dir.and_then(|dir| schedule_install_dir_removal(&dir)));
    record_step(&mut failed, "data folder", remove_data_dir());
    // Written after the data folder is gone, so a failed uninstall still leaves one line saying what is left.
    if !failed.is_empty() {
        log(&format!("uninstall finished with failed steps: {}", failed.join(", ")));
    }
}

/// Whether the per-user Run key starts this app at sign-in.
fn autostart_enabled() -> bool {
    windows_registry::CURRENT_USER.open(RUN_KEY).and_then(|key| key.get_string(RUN_VALUE)).is_ok()
}

/// Adds or removes the installed exe from the per-user Run key (`--autostart` starts silently in the tray).
fn set_autostart(enable: bool) -> Result<(), String> {
    let key = windows_registry::CURRENT_USER.create(RUN_KEY).map_err(|e| e.to_string())?;
    if enable {
        let exe = canonical_install_exe()
            .filter(|p| p.exists())
            .or_else(|| std::env::current_exe().ok())
            .ok_or("cannot resolve executable path")?;
        key.set_string(RUN_VALUE, format!("\"{}\" {AUTOSTART_ARG}", exe.display()))
            .map_err(|e| e.to_string())
    } else {
        match key.remove_value(RUN_VALUE) {
            // Removing a value that was never there fails too; only a value still present is a real failure.
            Err(e) if key.get_string(RUN_VALUE).is_ok() => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// False when another Desk Pager is already running; signals the running instance to show its window.
fn claim_single_instance() -> bool {
    // SAFETY: the mutex handle is deliberately leaked so it lives as long as the process.
    unsafe {
        let created = CreateMutexW(None, true, w!("Local\\DeskPagerSingleInstance"));
        if created.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        if let Ok(evt) = CreateEventW(None, false, false, w!("Local\\DeskPagerShowWindowEvent")) {
            let _ = SetEvent(evt);
        }
        false
    }
}

/// Listens for `Local\DeskPagerShowWindowEvent` so launching Desk Pager while it is in the tray opens the window.
fn spawn_show_window_listener(app: AppHandle) {
    std::thread::spawn(move || {
        // SAFETY: standard Win32 auto-reset named event waited on a dedicated background thread.
        unsafe {
            let Ok(evt) = CreateEventW(None, false, false, w!("Local\\DeskPagerShowWindowEvent")) else {
                return;
            };
            loop {
                if WaitForSingleObject(evt, INFINITE) == WAIT_OBJECT_0 {
                    open_pairing(&app);
                } else {
                    break;
                }
            }
        }
    });
}

fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Logs a failed UI update instead of dropping it.
fn report<E: std::fmt::Display>(result: Result<(), E>, what: &str) {
    if let Err(e) = result {
        log(&format!("{what} failed: {e}"));
    }
}

/// Appends one timestamped line to %LOCALAPPDATA%\DeskPager\desk-pager.log.
pub fn log(msg: &str) {
    // SAFETY: plain Win32 getter.
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {msg}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let written = std::env::var_os("LOCALAPPDATA").and_then(|dir| {
        let dir = PathBuf::from(dir).join(APP_DIR);
        std::fs::create_dir_all(&dir).ok()?;
        OpenOptions::new().create(true).append(true).open(dir.join(LOG_FILE)).ok()?.write_all(line.as_bytes()).ok()
    });
    if written.is_none() {
        eprint!("desk-pager log write failed: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// Builds a queued message whose body tells otherwise identical messages apart.
    fn msg(title: &'static str, body: &str) -> Outgoing {
        Outgoing { title, body: body.to_string() }
    }

    #[test]
    fn sender_thread_keeps_order_drops_nothing_under_the_cap_and_never_blocks_the_caller() {
        let (tx, rx) = mpsc::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&sent);
        let sender = std::thread::spawn(move || {
            run_sender(rx, |m| {
                // A slow network send: later messages must wait their turn, not be skipped.
                std::thread::sleep(Duration::from_millis(30));
                sink.lock().unwrap().push(m);
            })
        });

        let queued = [
            msg("Time for a long break", "1"),
            msg("Quick stretch & eye rest", "2"),
            msg(CLEAR_TITLE, "3"),
            msg("Desk Pager test", "4"),
            msg(CLEAR_TITLE, "5"),
        ];
        let t0 = Instant::now();
        for m in &queued {
            tx.send(msg(m.title, &m.body)).unwrap();
        }
        assert!(t0.elapsed() < Duration::from_millis(25), "queueing must not wait for a send");

        drop(tx);
        sender.join().unwrap();
        assert_eq!(*sent.lock().unwrap(), queued);
    }

    #[test]
    fn send_queue_drops_only_the_oldest_buzz_past_the_cap_and_never_a_clear() {
        let mut q = SendQueue::default();
        assert_eq!(q.push(msg(CLEAR_TITLE, "clear-0")), None);
        for i in 1..MAX_PENDING_SENDS {
            assert_eq!(q.push(msg("buzz", &i.to_string())), None, "nothing dropped up to the cap");
        }
        // One past the cap: the oldest buzz goes, the older clear stays.
        assert_eq!(q.push(msg("buzz", "new")), Some(msg("buzz", "1")));
        assert_eq!(q.pending.len(), MAX_PENDING_SENDS);
        assert_eq!(q.pop(), Some(msg(CLEAR_TITLE, "clear-0")));
        assert_eq!(q.pop(), Some(msg("buzz", "2")));
        assert_eq!(q.pending.back(), Some(&msg("buzz", "new")));

        // A queue of nothing but clears grows instead of losing one.
        let mut q = SendQueue::default();
        for i in 0..=MAX_PENDING_SENDS {
            assert_eq!(q.push(msg(CLEAR_TITLE, &i.to_string())), None);
        }
        assert_eq!(q.pending.len(), MAX_PENDING_SENDS + 1);
    }

    #[test]
    fn install_action_covers_every_branch() {
        use InstallAction::*;
        // Explicit --install always forces a full first install.
        assert_eq!(install_action(true, true, true, true), FirstInstall);
        assert_eq!(install_action(false, false, true, false), FirstInstall);
        // A Cargo build tree never auto-installs.
        assert_eq!(install_action(false, false, false, true), RunInPlace);
        assert_eq!(install_action(true, false, false, true), RunInPlace);
        // Nothing installed yet.
        assert_eq!(install_action(false, false, false, false), FirstInstall);
        // Same binary already installed: no reinstall.
        assert_eq!(install_action(true, true, false, false), ShowInstalled);
        // A different binary is installed.
        assert_eq!(install_action(true, false, false, false), Upgrade);
    }

    #[test]
    fn files_identical_compares_length_then_contents() {
        let dir = std::env::temp_dir().join(format!("deskpager-test-identical-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::write(&a, b"same bytes").unwrap();
        std::fs::write(&b, b"same bytes").unwrap();
        assert!(files_identical(&a, &b).unwrap());
        std::fs::write(&b, b"same bytez").unwrap();
        assert!(!files_identical(&a, &b).unwrap(), "same length, different contents");
        std::fs::write(&b, b"longer than a").unwrap();
        assert!(!files_identical(&a, &b).unwrap());
        assert!(files_identical(&a, &dir.join("missing")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn system_dir_needs_an_absolute_system_root() {
        assert_eq!(
            system_dir_from(Some(OsStr::new(r"C:\Windows"))).map(|d| d.join(POWERSHELL_EXE)),
            Ok(PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"))
        );
        assert!(system_dir_from(None).is_err());
        assert!(system_dir_from(Some(OsStr::new("Windows"))).is_err());
        assert!(system_dir_from(Some(OsStr::new(""))).is_err());
    }

    #[test]
    fn only_the_canonical_install_dir_may_be_removed() {
        let base = Path::new(r"C:\Users\A B\AppData\Local");
        assert_eq!(
            checked_install_dir(&base.join("Programs").join("DeskPager"), base),
            Ok(r"C:\Users\A B\AppData\Local\Programs\DeskPager".to_string())
        );
        assert!(checked_install_dir(&base.join("Programs"), base).is_err());
        assert!(checked_install_dir(base, base).is_err());
        assert!(checked_install_dir(Path::new(r"C:\Windows"), base).is_err());
        // An empty or relative LOCALAPPDATA would make rmdir act relative to the working directory.
        let relative = Path::new("");
        assert!(checked_install_dir(&relative.join("Programs").join("DeskPager"), relative).is_err());
        let dotted = Path::new(r"C:\Users\x\..\y");
        assert!(checked_install_dir(&dotted.join("Programs").join("DeskPager"), dotted).is_err());
        let expanding = Path::new(r"C:\Users\%USERNAME%");
        assert!(checked_install_dir(&expanding.join("Programs").join("DeskPager"), expanding).is_err());
    }

    #[test]
    fn rmdir_command_line_uses_cmd_quoting() {
        assert_eq!(
            rmdir_command_line(Path::new(r"C:\Windows\System32\ping.exe"), r"C:\Users\A B\Programs\DeskPager"),
            r#"/C ""C:\Windows\System32\ping.exe" 127.0.0.1 -n 2 > nul & rmdir /S /Q "C:\Users\A B\Programs\DeskPager"""#
        );
    }

    #[test]
    fn rmdir_command_line_really_removes_a_folder_with_spaces() {
        let dir = std::env::temp_dir().join(format!("deskpager test rmdir {}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("file.txt"), b"x").unwrap();
        let system_dir = system_dir().unwrap();
        let status = Command::new(system_dir.join(CMD_EXE))
            .raw_arg(rmdir_command_line(&system_dir.join(PING_EXE), dir.to_str().unwrap()))
            .current_dir(&system_dir)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(!dir.exists(), "cmd must have understood the quoting and removed the folder");
    }

    #[test]
    fn capture_errors_are_logged_only_on_a_state_change() {
        assert_eq!(capture_transition(false, None), None);
        assert!(capture_transition(false, Some("E_FAIL")).is_some_and(|line| line.ends_with("E_FAIL")));
        assert_eq!(capture_transition(true, Some("E_FAIL")), None, "no repeat every tick");
        assert_eq!(capture_transition(true, None), Some("call detection recovered".to_string()));
    }

    #[test]
    fn validates_break_after_secs_arg() {
        let args = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(parse_break_after_secs(&args(&["desk-pager"])), Ok(None));
        assert_eq!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs", "60"])), Ok(Some(60)));
        assert_eq!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs", "10"])), Ok(Some(10)));
        assert_eq!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs", "5400"])), Ok(Some(5400)));
        assert!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs"])).is_err());
        assert!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs", "9"])).is_err());
        assert!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs", "5401"])).is_err());
        assert!(parse_break_after_secs(&args(&["desk-pager", "--break-after-secs", "abc"])).is_err());
    }

    #[test]
    fn round_trips_and_clamps_schedule_settings() {
        let custom = ScheduleSettings {
            short_every_mins: 45,
            short_break_secs: 30,
            long_every_mins: 120,
            long_break_mins: 15,
            desktop_notify: false,
        };
        let text = custom.to_cfg_text();
        assert_eq!(ScheduleSettings::from_cfg_text(&text), custom);

        let out_of_range = ScheduleSettings {
            short_every_mins: 0,
            short_break_secs: 2,
            long_every_mins: 2,
            long_break_mins: 99,
            desktop_notify: true,
        }
        .sanitized();
        assert_eq!(out_of_range.short_every_mins, 1);
        assert_eq!(out_of_range.short_break_secs, 5);
        assert_eq!(out_of_range.long_every_mins, 5);
        assert_eq!(out_of_range.long_break_mins, 60);
        assert!(out_of_range.desktop_notify);
    }
}
