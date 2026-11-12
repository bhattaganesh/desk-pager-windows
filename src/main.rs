#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Desk Pager: a tray app that buzzes your phone (through ntfy) after 45 minutes of continuous sitting.

mod core;
mod secret;
mod sensors;

use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use qrcode::{render::svg, QrCode};
use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{TrayIcon, TrayIconBuilder},
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

const MENU_PAIR: &str = "pair";
const MENU_TEST: &str = "test";
const MENU_PAUSE: &str = "pause";
const MENU_AUTOSTART: &str = "autostart";
const MENU_QUIT: &str = "quit";

/// Shared between the tray menu, the pairing window and the timer thread.
struct AppState {
    topic: String,
    paused: AtomicBool,
}

/// What the pairing window shows.
#[derive(Serialize)]
struct PairingInfo {
    code: String,
    qr_svg: String,
}

/// Returns the pairing code and a QR code of the `deskpager://` pairing link.
#[tauri::command]
fn pairing_info(state: State<'_, AppState>) -> Result<PairingInfo, String> {
    let link = format!("{PAIR_LINK_PREFIX}{}", state.topic);
    let qr = QrCode::new(link.as_bytes()).map_err(|e| format!("QR code failed: {e}"))?;
    let qr_svg = qr.render::<svg::Color>().min_dimensions(QR_SIZE_PX, QR_SIZE_PX).build();
    Ok(PairingInfo { code: state.topic.clone(), qr_svg })
}

/// Called by the pairing page once it has drawn itself, so a blank window shows up in the log.
#[tauri::command]
fn pairing_rendered(qr_shown: bool) {
    log(&format!("pairing window rendered, qr shown: {qr_shown}"));
}

fn main() {
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
    let app = tauri::Builder::default()
        .manage(AppState { topic: topic.value, paused: AtomicBool::new(false) })
        .invoke_handler(tauri::generate_handler![pairing_info, pairing_rendered])
        .setup(move |app| {
            let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
            let pair = MenuItem::with_id(app, MENU_PAIR, "Pair phone…", true, None::<&str>)?;
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

            let (pause_item, autostart_item) = (pause.clone(), autostart.clone());
            let tray = TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().ok_or("bundle icon missing")?)
                .tooltip("Desk Pager")
                .menu(&menu)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    MENU_PAIR => open_pairing(app),
                    MENU_TEST => {
                        let app = app.clone();
                        std::thread::spawn(move || {
                            deliver(&app, "Desk Pager test", "Test buzz from your laptop.".into());
                        });
                    }
                    MENU_PAUSE => {
                        let state = app.state::<AppState>();
                        let paused = !state.paused.fetch_xor(true, Ordering::Relaxed);
                        report(pause_item.set_text(if paused { "Resume" } else { "Pause" }), "pause label");
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
            std::thread::spawn(move || run_timer(&handle, &status, &tray));
            if first_run {
                open_pairing(app.handle());
            }
            if std::env::args().any(|arg| arg == TEST_BUZZ_ARG) {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    deliver(&handle, "Desk Pager test", "Test buzz from your laptop.".into());
                });
            }
            log(&format!("started, first run: {first_run}"));
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
fn run_timer(app: &AppHandle, status: &MenuItem<tauri::Wry>, tray: &TrayIcon) {
    let cfg = core::Config::default();
    let mut tracker = core::Tracker::default();
    let mut suspends_seen = sensors::suspend_count();
    let mut shown = String::new();
    loop {
        let now = unix_secs();
        let state = app.state::<AppState>();
        let text = if state.paused.load(Ordering::Relaxed) {
            tracker.reset();
            "Paused".to_string()
        } else {
            let suspends = sensors::suspend_count();
            if suspends != suspends_seen {
                suspends_seen = suspends;
                tracker.reset();
            }
            match sensors::idle_secs() {
                Ok(idle) => {
                    if let Some(alert) = tracker.tick(&cfg, now, idle, sensors::is_locked()) {
                        let body = format!(
                            "You've been sitting for {} minutes. Stand up and move for a bit.",
                            alert.minutes
                        );
                        deliver(app, "Time to stand up", body);
                    }
                    format!("Sitting: {} min", tracker.minutes(now))
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

/// Sends one buzz to the phone, retrying a few times, and logs the outcome (never the topic).
fn deliver(app: &AppHandle, title: &str, body: String) {
    let topic = app.state::<AppState>().topic.clone();
    for attempt in 1..=SEND_ATTEMPTS {
        match tauri::async_runtime::block_on(send(&topic, title, body.clone())) {
            Ok(()) => return log(&format!("buzz sent: {title}")),
            Err(e) => log(&format!("buzz failed (attempt {attempt}/{SEND_ATTEMPTS}): {e}")),
        }
        std::thread::sleep(SEND_RETRY_WAIT);
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
            .title("Desk Pager — pair your phone")
            .inner_size(420.0, 620.0)
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
