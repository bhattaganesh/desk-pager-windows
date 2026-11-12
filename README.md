# Desk Pager for Windows

A tray app that buzzes your phone after 45 minutes of continuous sitting at your PC.
The phone side is the Desk Pager Android app; the two talk through [ntfy.sh](https://ntfy.sh) (free, no account).

## Use it

1. Download `desk-pager.exe` from the latest release and run it. It lives in the system tray (check the `^` overflow).
2. On first run a pairing window opens. Scan the QR code with your phone, or paste the pairing code into the Android app.
3. Right-click the tray icon → **Send test buzz** to check the phone gets it.
4. Optional: tick **Start with Windows** in the tray menu.

Requires Windows 10/11 with the WebView2 runtime (preinstalled on Windows 11).

## Two-Tier Break Rules (Customizable in App)

| Rule | Default | Customizable Options |
|---|---|---|
| **Tier 1 — Quick Stretch (Stretch, Eyes & Water)** | every **30 min** (pause **30 s**) | `15, 20, 30, 40, 45, 60 min` · pause `20s, 30s, 45s, 60s` |
| **Tier 2 — Long Break (Walk & Back Recovery)** | every **120 min / 2 hr** (break **10 min**) | `60, 90, 120, 150, 180 min` · break `5, 10, 15, 20 min` |
| Repeat buzzes | every 10 min while you keep sitting, 3 per cycle at most | — |
| Quick stretch reset | pausing keyboard/mouse for the stretch duration (`30 s`) after a buzz, or clicking **Took stretch** (resets Tier 1 while Tier 2 keeps counting) | — |
| Full session reset | stepping away for the long-break duration (`10 min`), locking the PC for 2+ min, or PC sleep (resets both Tier 1 & Tier 2) | — |
| Call hold | holds due buzzes while a call app uses the mic + 30 s grace after, up to 45 min max | — |
| Pause | tray menu → **Pause** (or **Pause** button in the dashboard) | — |

Schedule preferences are saved locally in `%LOCALAPPDATA%\DeskPager\settings.cfg` and apply immediately without restarting. Dictation and system audio tools (`Wispr Flow.exe`, `VoiceAccess.exe`, `SpeechRuntime.exe`, `TextInputHost.exe`, `NVIDIA Broadcast.exe`, `audiodg.exe`) are ignored so voice typing does not delay buzzes. For listen-only meetings where your microphone is never opened, use **Pause**.

## Privacy and security

- The pairing code is a random 128-bit ntfy topic. Anyone who has it can send buzzes to your phone, so don't share it.
- It is stored in Windows Credential Manager (`DeskPager/ntfy-topic`), never in a file or the log.
- The app sends nothing except the buzz text to `https://ntfy.sh`. No keystrokes, window titles or usage data leave the PC.
- Log: `%LOCALAPPDATA%\DeskPager\desk-pager.log`.

## Build

Needs Rust (stable, MSVC toolchain) and the Microsoft C++ Build Tools.

```
cargo test --features tauri/custom-protocol
cargo build --release --features tauri/custom-protocol
```

The exe is `target\release\desk-pager.exe`. `desk-pager.exe --test-buzz` sends one test buzz at start-up; `desk-pager.exe --break-after-secs 60` overrides the first-buzz threshold (`10..=5400` s) for end-to-end testing.

The exe is not code-signed, so Windows SmartScreen may warn the first time it runs.
