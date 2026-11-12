#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Desk Pager: a tray app that buzzes your phone (through ntfy) after 45 minutes of continuous sitting.

mod core;
mod secret;
mod sensors;

use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
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
    core::w,
    Win32::{
        Foundation::{GetLastError, ERROR_ALREADY_EXISTS},
        System::{SystemInformation::GetLocalTime, Threading::CreateMutexW},
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
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DeskPager";
const LOG_FILE: &str = "desk-pager.log";
const TEST_BUZZ_ARG: &str = "--test-buzz";
const OPEN_PAIR_ARG: &str = "--open-pair";
const BREAK_AFTER_SECS_ARG: &str = "--break-after-secs";
const MIN_BREAK_AFTER_SECS: u64 = 10;
const MAX_BREAK_AFTER_SECS: u64 = 5400;

const MENU_PAIR: &str = "pair";
const MENU_TEST: &str = "test";
const MENU_PAUSE: &str = "pause";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";

/// Shared between the tray menu, the pairing window and the timer thread.
struct AppState {
    topic: String,
    paused: AtomicBool,
    sending: Arc<AtomicBool>,
    sitting_secs: AtomicU64,
    break_after_secs: u64,
    call_app: Mutex<Option<String>>,
    pause_item: Mutex<Option<MenuItem<tauri::Wry>>>,
}

/// What the pairing window shows.
#[derive(Serialize)]
struct PairingInfo {
    code: String,
    qr_svg: String,
}

/// Live timer and sensor status for the desktop window.
#[derive(Serialize)]
struct StatusInfo {
    paused: bool,
    sitting_secs: u64,
    sitting_minutes: u64,
    break_after_secs: u64,
    call_app: Option<String>,
}

/// Returns the pairing code and a QR code of the `deskpager://` pairing link.
#[tauri::command]
fn pairing_info(state: State<'_, AppState>) -> Result<PairingInfo, String> {
    let link = format!("{PAIR_LINK_PREFIX}{}", state.topic);
    let qr = QrCode::new(link.as_bytes()).map_err(|e| format!("QR code failed: {e}"))?;
    let qr_svg = qr.render::<svg::Color>().min_dimensions(QR_SIZE_PX, QR_SIZE_PX).build();
    Ok(PairingInfo { code: state.topic.clone(), qr_svg })
}

/// Returns live timer state for the desktop dashboard window.
#[tauri::command]
fn app_status(state: State<'_, AppState>) -> StatusInfo {
    let sitting_secs = state.sitting_secs.load(Ordering::Relaxed);
    let call_app = state.call_app.lock().ok().and_then(|g| g.clone());
    StatusInfo {
        paused: state.paused.load(Ordering::Relaxed),
        sitting_secs,
        sitting_minutes: sitting_secs / 60,
        break_after_secs: state.break_after_secs,
        call_app,
    }
}

/// Toggles the pause state from either the window or tray menu and syncs the tray label.
#[tauri::command]
fn toggle_pause(state: State<'_, AppState>) -> bool {
    let paused = !state.paused.fetch_xor(true, Ordering::Relaxed);
    if paused {
        state.sitting_secs.store(0, Ordering::Relaxed);
    }
    if let Ok(guard) = state.pause_item.lock() {
        if let Some(item) = guard.as_ref() {
            report(item.set_text(if paused { "Resume" } else { "Pause" }), "pause label");
        }
    }
    paused
}

/// Sends a test buzz from the desktop window button.
#[tauri::command]
fn send_test_buzz(app: AppHandle) {
    spawn_deliver(&app, "Desk Pager test", "Test buzz from your laptop.".into());
}

/// Called by the pairing page once it has drawn itself, so a blank window shows up in the log.
#[tauri::command]
fn pairing_rendered(qr_shown: bool) {
    log(&format!("pairing window rendered, qr shown: {qr_shown}"));
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
    let args: Vec<String> = std::env::args().collect();
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
    sensors::spawn();

    let first_run = topic.is_new;
    let mut cfg = core::Config::default();
    if let Some(secs) = break_override {
        cfg.break_after = secs;
    }
    let app = tauri::Builder::default()
        .manage(AppState {
            topic: topic.value,
            paused: AtomicBool::new(false),
            sending: Arc::new(AtomicBool::new(false)),
            sitting_secs: AtomicU64::new(0),
            break_after_secs: cfg.break_after,
            call_app: Mutex::new(None),
            pause_item: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            pairing_info,
            pairing_rendered,
            app_status,
            toggle_pause,
            send_test_buzz
        ])
        .setup(move |app| {
            let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
            let pair = MenuItem::with_id(app, MENU_PAIR, "Open Desk Pager…", true, None::<&str>)?;
            let test = MenuItem::with_id(app, MENU_TEST, "Send test buzz", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, MENU_PAUSE, "Pause", true, None::<&str>)?;
            let autostart = CheckMenuItem::with_id(
                app, MENU_AUTOSTART, "Start with Windows", true, autostart_enabled(), None::<&str>,
            )?;
            let quit = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[&status, &separator, &pair, &test, &pause, &autostart, &separator, &quit],
            )?;

            if let Ok(mut guard) = app.state::<AppState>().pause_item.lock() {
                *guard = Some(pause.clone());
            }
            let autostart_item = autostart.clone();
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
                    MENU_TEST => spawn_deliver(app, "Desk Pager test", "Test buzz from your laptop.".into()),
                    MENU_PAUSE => {
                        let _ = toggle_pause(app.state::<AppState>());
                    }
                    MENU_AUTOSTART => {
                        let enable = !autostart_enabled();
                        if let Err(e) = set_autostart(enable) {
                            log(&format!("autostart change failed: {e}"));
                        }
                        report(autostart_item.set_checked(autostart_enabled()), "autostart check");
                    }
                    MENU_QUIT => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            let handle = app.handle().clone();
            std::thread::spawn(move || run_timer(&handle, &status, &tray, cfg));
            if first_run || args.iter().any(|arg| arg == OPEN_PAIR_ARG) {
                open_pairing(app.handle());
            }
            if args.iter().any(|arg| arg == TEST_BUZZ_ARG) {
                spawn_deliver(app.handle(), "Desk Pager test", "Test buzz from your laptop.".into());
            }
            log(&format!("started, first run: {first_run}, break_after: {}s", cfg.break_after));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build tauri app");

    app.run(|_, event| {
        // code None = "last window closed": stay in the tray. Some = Quit, which must still exit.
        if let RunEvent::ExitRequested { code: None, api, .. } = event {
            api.prevent_exit();
        }
    });
}

/// Samples the sensors every few seconds, drives the sitting timer and keeps the tray text current.
fn run_timer(app: &AppHandle, status: &MenuItem<tauri::Wry>, tray: &TrayIcon, cfg: core::Config) {
    let mut tracker = core::Tracker::default();
    let mut suspends_seen = sensors::suspend_count();
    let mut shown = String::new();
    loop {
        let now = unix_secs();
        let state = app.state::<AppState>();
        let text = if state.paused.load(Ordering::Relaxed) {
            tracker.reset();
            state.sitting_secs.store(0, Ordering::Relaxed);
            if let Ok(mut guard) = state.call_app.lock() {
                *guard = None;
            }
            "Paused".to_string()
        } else {
            let suspends = sensors::suspend_count();
            if suspends != suspends_seen {
                suspends_seen = suspends;
                tracker.reset();
            }
            match sensors::idle_secs() {
                Ok(idle) => {
                    let apps = sensors::capture_apps().unwrap_or_default();
                    let call_app = core::active_call_app(&apps);
                    if let Ok(mut guard) = state.call_app.lock() {
                        *guard = call_app.map(str::to_string);
                    }
                    if let Some(alert) = tracker.tick(&cfg, now, idle, sensors::is_locked(), call_app.is_some()) {
                        let body = format!(
                            "You've been sitting for {} minutes. Stand up and move for a bit.",
                            alert.minutes
                        );
                        spawn_deliver(app, "Time to stand up", body);
                    }
                    state.sitting_secs.store(tracker.seconds(now), Ordering::Relaxed);
                    match call_app {
                        Some(name) => format!("Sitting: {} min (in call: {name})", tracker.minutes(now)),
                        None => format!("Sitting: {} min", tracker.minutes(now)),
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
            report(tray.set_tooltip(Some(format!("Desk Pager — {text}"))), "tooltip");
            shown = text;
        }
        std::thread::sleep(POLL);
    }
}

/// Runs `work` on a background thread unless another send is already in flight.
fn spawn_single_flight<F>(in_flight: &Arc<AtomicBool>, work: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    if in_flight.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return false;
    }
    let flag = Arc::clone(in_flight);
    std::thread::spawn(move || {
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = Guard(flag);
        work();
    });
    true
}

/// Spawns a non-overlapping background thread to deliver one buzz, so the timer thread never blocks.
fn spawn_deliver(app: &AppHandle, title: &'static str, body: String) {
    let state = app.state::<AppState>();
    let topic = state.topic.clone();
    if !spawn_single_flight(&state.sending, move || deliver(&topic, title, body)) {
        log(&format!("buzz skipped (send already in progress): {title}"));
    }
}

/// Sends one buzz to the phone, retrying a few times, and logs the outcome (never the topic).
fn deliver(topic: &str, title: &str, body: String) {
    for attempt in 1..=SEND_ATTEMPTS {
        match tauri::async_runtime::block_on(send(topic, title, body.clone())) {
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

/// Opens (or focuses) the pairing window; it is destroyed when closed.
fn open_pairing(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(PAIR_WINDOW) {
        report(window.set_focus(), "pairing focus");
        return;
    }
    let app = app.clone();
    // Building a webview from a main-thread event handler can deadlock on Windows; build off-thread.
    tauri::async_runtime::spawn(async move {
        let built = WebviewWindowBuilder::new(&app, PAIR_WINDOW, WebviewUrl::App("index.html".into()))
            .title("Desk Pager")
            .inner_size(440.0, 680.0)
            .resizable(false)
            .build();
        if let Err(e) = built {
            log(&format!("pairing window failed: {e}"));
        }
    });
}

/// Whether the per-user Run key starts this app at sign-in.
fn autostart_enabled() -> bool {
    windows_registry::CURRENT_USER.open(RUN_KEY).and_then(|key| key.get_string(RUN_VALUE)).is_ok()
}

/// Adds or removes this exe from the per-user Run key.
fn set_autostart(enable: bool) -> Result<(), String> {
    let key = windows_registry::CURRENT_USER.create(RUN_KEY).map_err(|e| e.to_string())?;
    if enable {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_string(RUN_VALUE, format!("\"{}\"", exe.display())).map_err(|e| e.to_string())
    } else {
        key.remove_value(RUN_VALUE).map_err(|e| e.to_string())
    }
}

/// False when another Desk Pager is already running (two would double every buzz).
fn claim_single_instance() -> bool {
    // SAFETY: the mutex handle is deliberately leaked so it lives as long as the process.
    unsafe {
        let created = CreateMutexW(None, true, w!("Local\\DeskPagerSingleInstance"));
        created.is_ok() && GetLastError() != ERROR_ALREADY_EXISTS
    }
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
        let dir = PathBuf::from(dir).join("DeskPager");
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
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    #[test]
    fn spawned_single_flight_delivery_never_stalls_timer_or_piles_up() {
        let cfg = core::Config::default();
        let mut tracker = core::Tracker::default();
        for now in (0..2700).step_by(5) {
            assert_eq!(tracker.tick(&cfg, now, 0, false, false), None);
        }
        assert_eq!(tracker.tick(&cfg, 2700, 0, false, false), Some(core::Alert { minutes: 45, number: 1 }));

        let sending = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(AtomicU32::new(0));
        let ran1 = Arc::clone(&ran);
        let t0 = Instant::now();
        assert!(spawn_single_flight(&sending, move || {
            ran1.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(80));
        }));
        // Must return immediately without blocking the timer thread.
        assert!(t0.elapsed() < Duration::from_millis(40));

        // While the first send is in flight, a second send must be skipped instead of piling up.
        let ran2 = Arc::clone(&ran);
        assert!(!spawn_single_flight(&sending, move || {
            ran2.fetch_add(1, Ordering::Relaxed);
        }));

        // Timer ticks on schedule 5s later; sitting session stays intact and reaches 46 min at 2760s.
        for now in (2705..=2760).step_by(5) {
            assert_eq!(tracker.tick(&cfg, now, 0, false, false), None);
        }
        assert_eq!(tracker.minutes(2760), 46);

        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ran.load(Ordering::Relaxed), 1);
        assert!(!sending.load(Ordering::Acquire));
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
}
