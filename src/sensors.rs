//! Windows inputs for the timer: idle time, session lock state, suspend notifications and active mic capture.

use std::{
    ffi::c_void,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use windows::{
    core::{w, Error, Interface, Result, HRESULT, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        Media::Audio::{
            eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionEnumerator,
            IAudioSessionManager2, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator,
            DEVICE_STATE_ACTIVE,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED},
            LibraryLoader::GetModuleHandleW,
            Power::{RegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS},
            RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION},
            SystemInformation::GetTickCount,
            Threading::{
                OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
                PROCESS_QUERY_LIMITED_INFORMATION,
            },
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

const MAX_PATH_WIDE: usize = 1024;
const RPC_E_CHANGED_MODE_HR: HRESULT = HRESULT(0x80010106u32 as i32);

static LOCKED: AtomicBool = AtomicBool::new(false);
static SUSPENDS: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static COM_INIT: Result<()> = {
        // SAFETY: STA is required on the timer thread because Shell_NotifyIconW (tray tooltip) fails under MTA.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.is_ok() || hr == RPC_E_CHANGED_MODE_HR {
            Ok(())
        } else {
            Err(Error::from(hr))
        }
    };
}

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

/// Result of one microphone capture scan: the apps found, plus the first error if anything was skipped.
#[derive(Default)]
pub struct CaptureScan {
    /// Executable names owning an Active audio session on any active microphone capture endpoint.
    pub apps: Vec<String>,
    /// First failure met while scanning; `apps` still holds everything that could be read.
    pub first_error: Option<Error>,
}

impl CaptureScan {
    /// Remembers the first failure of this scan.
    fn note(&mut self, error: Error) {
        self.first_error.get_or_insert(error);
    }
}

/// Scans every active microphone capture endpoint, skipping any endpoint or session that fails.
pub fn capture_apps() -> CaptureScan {
    let mut scan = CaptureScan::default();
    if let Err(e) = scan_endpoints(&mut scan) {
        scan.note(e);
    }
    scan.apps.sort();
    scan.apps.dedup();
    scan
}

/// Walks all capture endpoints into `scan`; returns `Err` only when the endpoint list itself is unavailable.
fn scan_endpoints(scan: &mut CaptureScan) -> Result<()> {
    COM_INIT.with(|init| init.clone())?;
    // SAFETY: COM is initialised for this thread; every interface lives within its local scope.
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let devices = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        for d in 0..devices.GetCount()? {
            let sessions = match endpoint_sessions(&devices, d) {
                Ok(sessions) => sessions,
                Err(e) => {
                    scan.note(e);
                    continue;
                }
            };
            let count = match sessions.GetCount() {
                Ok(count) => count,
                Err(e) => {
                    scan.note(e);
                    continue;
                }
            };
            for s in 0..count {
                match active_session_pid(&sessions, s) {
                    Ok(Some(pid)) => scan.apps.push(process_name(pid)),
                    Ok(None) => {}
                    Err(e) => scan.note(e),
                }
            }
        }
    }
    Ok(())
}

/// Opens the audio session list of capture endpoint `d`.
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn endpoint_sessions(devices: &IMMDeviceCollection, d: u32) -> Result<IAudioSessionEnumerator> {
    let manager: IAudioSessionManager2 = devices.Item(d)?.Activate(CLSCTX_ALL, None)?;
    manager.GetSessionEnumerator()
}

/// Returns the owning process id of session `s` when that session is Active.
///
/// # Safety
/// COM must be initialised on the calling thread.
unsafe fn active_session_pid(sessions: &IAudioSessionEnumerator, s: i32) -> Result<Option<u32>> {
    let control = sessions.GetSession(s)?;
    if control.GetState()? != AudioSessionStateActive {
        return Ok(None);
    }
    Ok(Some(control.cast::<IAudioSessionControl2>()?.GetProcessId()?))
}

fn process_name(pid: u32) -> String {
    // SAFETY: `buf` outlives `QueryFullProcessImageNameW` and `handle` is closed before returning.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return format!("pid:{pid}");
        };
        let mut buf = [0u16; MAX_PATH_WIDE];
        let mut len = buf.len() as u32;
        let queried = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        match queried {
            Ok(()) => {
                let path = String::from_utf16_lossy(&buf[..len as usize]);
                path.rsplit('\\').next().unwrap_or(&path).to_string()
            }
            Err(_) => format!("pid:{pid}"),
        }
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
