# Desk Pager for Windows (`v0.5.0`)

A lightweight Windows system-tray application that tracks continuous sitting with context awareness (input idle, workstation lock/sleep, and active microphone calls) and delivers two-tier posture, eye-rest, and walk reminders to your Android phone via [ntfy.sh](https://ntfy.sh) (free, no account) plus an optional silent Windows 11 Notification Center alert.

## Required Downloads (Desktop + Mobile)

To use Desk Pager, you need **both** the Windows desktop application on your laptop(s) and the companion Android application on your phone. Multiple releases are available — always download the **latest version** from the links below:

1. **Windows Desktop Application (`desk-pager.exe`):**
   - **Latest Release (`v0.5.0`):** [https://github.com/bhattaganesh/desk-pager-windows/releases/latest](https://github.com/bhattaganesh/desk-pager-windows/releases/latest)
   - **All Windows Releases:** [https://github.com/bhattaganesh/desk-pager-windows/releases](https://github.com/bhattaganesh/desk-pager-windows/releases)
2. **Companion Android Mobile Application (`desk-pager.apk` — Required on your phone):**
   - **Latest Release (`v0.4.0`):** [https://github.com/bhattaganesh/desk-pager-android/releases/latest](https://github.com/bhattaganesh/desk-pager-android/releases/latest)
   - **All Android Releases:** [https://github.com/bhattaganesh/desk-pager-android/releases](https://github.com/bhattaganesh/desk-pager-android/releases)

---

## Quick Start

1. Download `desk-pager.exe` from the [latest Windows release](https://github.com/bhattaganesh/desk-pager-windows/releases/latest) and double-click it:
   - **Automatic 1-Click Setup:** When launched from your `Downloads` folder, `desk-pager.exe` automatically installs itself to `%LOCALAPPDATA%\Programs\DeskPager\desk-pager.exe`, creates **Start Menu** and **Desktop** shortcuts, registers under **Windows Settings → Apps → Installed apps**, enables **Start with Windows**, and opens with an **"Installation Complete"** confirmation banner. You can safely delete the downloaded `.exe` from your `Downloads` folder afterward.
2. Download and install `desk-pager.apk` on your Android phone from the [latest Android release](https://github.com/bhattaganesh/desk-pager-android/releases/latest).
3. On your PC, switch to the **Phone Pairing** tab, scan the QR code with your Android phone (or copy and paste the `deskpager-…` secret code into the Android app), and tap **Pair / Add PC**.
   - **Using Multiple Laptops (Home + Office):** Run `desk-pager.exe` on your second laptop, tap **`+ Add PC`** in the Android app, and scan the second laptop's QR code. Both laptops remain connected simultaneously without ever needing to re-enter codes.
4. Click **Send Test Buzz** to verify phone vibration and your Windows Notification Center alert.
5. **Background Tray Behavior:** Minimizing (`_`) or clicking Close (`X`) on the window hides Desk Pager into the Windows system tray while sitting tracking continues uninterrupted in the background. Left-click the tray icon (or open **Desk Pager** from the Start Menu / Desktop shortcut) anytime to reopen the dashboard. To completely close Desk Pager, right-click the tray icon and select **Quit**.

Requires Windows 10/11 with the WebView2 runtime (preinstalled on Windows 11).

## Two-Tier Break Rules & Controls

| Feature | Default | Custom Range / Behavior |
|---|---|---|
| **Tier 1 — Quick Stretch, Eye Rest & Water** | every **30 min** (pause **30 s**) | Custom number input, `−`/`+` steppers, or preset chips (`1–180 min` interval · `5–300 sec` pause) |
| **Tier 2 — Long Break (Walk & Back Recovery)** | every **120 min / 2 hr** (walk **10 min**) | Custom number input, `−`/`+` steppers, or preset chips (`5–360 min` interval · `1–60 min` walk) |
| **Repeat reminders** | every **10 min** while sitting | Up to 3 reminders per cycle |
| **Quick stretch reset** | **30 s** idle after buzz | Pausing keyboard/mouse for the stretch duration after a buzz (or clicking **Took Stretch**) resets Tier 1 while Tier 2 keeps counting |
| **Full 2-hour reset** | **10 min** away or **2+ min** lock | Stepping away for the walk duration, locking the PC for `2+ min`, PC sleep, or clicking **Reset 2h Cycle** resets both Tier 1 & Tier 2 |
| **Hands-free phone alarm clear** | **30 s** idle or PC lock | When a strict Long Break alarm is ringing on your phone, stepping away from your PC for `30+ s` (or locking it) sends `deskpager-clear` to stop the phone alarm automatically |
| **Microphone call hold** | Automatic (`Core Audio`) | Holds due reminders while a call app uses your mic + `30 s` grace after, up to `45 min` max (ignores dictation tools like `Wispr Flow.exe`, `VoiceAccess.exe`, `NVIDIA Broadcast.exe`) |
| **Meeting / presentation hold** | Manual toggle (`90 min` cap) | Click **In a Meeting** in the dashboard or tray menu to hold reminders during listen-only meetings or screen shares (auto-expires after 90 min) |
| **Silent PC Alert** | **Enabled** by default | Posts a silent (`<audio silent="true"/>`), non-focus-stealing notification in the Windows 11 Notification Center; uncheck **Silent PC Alert** for mobile-only mode |

Schedule preferences are saved locally in `%LOCALAPPDATA%\DeskPager\settings.cfg` and apply immediately without restarting.

## Privacy & Security

- Your pairing code is a cryptographically random 128-bit topic (`deskpager-` + 32 hex chars) stored in **Windows Credential Manager** (`DeskPager/ntfy-topic`), never in a config file or log.
- You can rotate the secret at any time from **Phone Pairing → Regenerate Secret Code**.
- Only reminder messages are sent to `https://ntfy.sh`. No keystrokes, window titles, or personal data ever leave your PC.
- Diagnostic log: `%LOCALAPPDATA%\DeskPager\desk-pager.log`.

## Build & Verify

Requires Rust (stable MSVC toolchain) and the Microsoft C++ Build Tools:

```powershell
cargo test --features tauri/custom-protocol
cargo clippy --release --features tauri/custom-protocol -- -D warnings
cargo build --release --features tauri/custom-protocol
```

The release binary is `target\release\desk-pager.exe`. Useful CLI flags:
- `desk-pager.exe --open-pair` — opens the dashboard window on launch.
- `desk-pager.exe --test-buzz` — fires a test notification and phone buzz on startup.
- `desk-pager.exe --break-after-secs 60` — overrides the Tier 1 interval (`10..=5400` s) for testing.
