# Desk Pager for Windows

A tray app that buzzes your phone after 45 minutes of continuous sitting at your PC.
The phone side is the Desk Pager Android app; the two talk through [ntfy.sh](https://ntfy.sh) (free, no account).

## Use it

1. Download `desk-pager.exe` from the latest release and run it. It lives in the system tray (check the `^` overflow).
2. On first run a pairing window opens. Scan the QR code with your phone, or paste the pairing code into the Android app.
3. Right-click the tray icon → **Send test buzz** to check the phone gets it.
4. Optional: tick **Start with Windows** in the tray menu.

Requires Windows 10/11 with the WebView2 runtime (preinstalled on Windows 11).

## Rules

| Rule | Value |
|---|---|
| First buzz | after 45 min of continuous use |
| Repeat buzzes | every 10 min while you keep sitting, 3 per session at most |
| A break | 5 min with no keyboard or mouse input, a lock of 2 min or more, or the PC sleeping |
| Call hold | holds due buzzes while a call app uses the mic + 30 s grace after, up to 45 min max |
| Pause | tray menu → **Pause** |

Dictation and system audio tools (`Wispr Flow.exe`, `VoiceAccess.exe`, `SpeechRuntime.exe`, `TextInputHost.exe`, `NVIDIA Broadcast.exe`, `audiodg.exe`) are ignored so voice typing does not delay buzzes. For listen-only meetings where your microphone is never opened, use **Pause**.

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
