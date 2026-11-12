//! Windows inputs for the timer: idle time, session lock state and suspend notifications.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use windows::{
    core::{w, Error, Result},
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        System::{
            LibraryLoader::GetModuleHandleW,
            Power::{RegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS},
            RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION},
            SystemInformation::GetTickCount,
        },
        UI::{
            Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
                DEVICE_NOTIFY_CALLBACK, MSG, PBT_APMSUSPEND, WINDOW_EX_STYLE, WM_WTSSESSION_CHANGE,
                WNDCLASSW, WS_OVERLAPPED, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
            },
        },
    },
};

static LOCKED: AtomicBool = AtomicBool::new(false);
static SUSPENDS: AtomicU32 = AtomicU32::new(0);

/// Starts the listener thread for lock/unlock and suspend events.
pub fn spawn() {
    std::thread::spawn(|| {
        if let Err(e) = run() {
            crate::log(&format!("sensor thread failed: {e}"));
        }
    });
}

/// Whether the session is locked right now (false until the first lock event is seen).
pub fn is_locked() -> bool {
    LOCKED.load(Ordering::Relaxed)
}

/// Number of suspends seen since start; a change means the PC slept.
pub fn suspend_count() -> u32 {
    SUSPENDS.load(Ordering::Relaxed)
}

/// Seconds since the last keyboard or mouse input in this session.
pub fn idle_secs() -> Result<u64> {
    let mut info = LASTINPUTINFO { cbSize: size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: `info` is a correctly sized LASTINPUTINFO that outlives the call.
    unsafe {
        GetLastInputInfo(&mut info).ok()?;
        // Both values are 32-bit tick counts, so wrapping subtraction survives the 49-day rollover.
        Ok(u64::from(GetTickCount().wrapping_sub(info.dwTime)) / 1000)
    }
}

fn run() -> Result<()> {
    // SAFETY: standard Win32 window setup; every pointer passed lives for the call or is leaked.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("DeskPagerSensor");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(Error::from_thread());
        }
        // Top-level but never shown: message-only windows miss session broadcasts.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0), class, w!("Desk Pager sensor"), WS_OVERLAPPED,
            0, 0, 0, 0, None, None, Some(instance.into()), None,
        )?;
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)?;

        // The OS calls back for the life of the process, so the parameters must never be freed.
        let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS =
            Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(power_callback),
                Context: std::ptr::null_mut(),
            }));
        RegisterSuspendResumeNotification(HANDLE(params as *mut _ as *mut c_void), DEVICE_NOTIFY_CALLBACK)?;

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_WTSSESSION_CHANGE {
        match wparam.0 as u32 {
            WTS_SESSION_LOCK => LOCKED.store(true, Ordering::Relaxed),
            WTS_SESSION_UNLOCK => LOCKED.store(false, Ordering::Relaxed),
            _ => {}
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe extern "system" fn power_callback(_context: *const c_void, kind: u32, _setting: *const c_void) -> u32 {
    if kind == PBT_APMSUSPEND {
        SUSPENDS.fetch_add(1, Ordering::Relaxed);
    }
    0
}
