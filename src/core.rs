//! The sitting timer: pure logic, no Windows calls, so it can be unit-tested.

/// Process names that open the microphone for dictation or system routing rather than a call.
const IGNORED_MIC_APPS: &[&str] = &[
    "wispr flow.exe",
    "voiceaccess.exe",
    "speechruntime.exe",
    "textinputhost.exe",
    "nvidia broadcast.exe",
    "audiodg.exe",
    "svchost.exe",
];

/// Timing rules, in seconds. Defaults follow docs/02-architecture.md §4.2.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Continuous sitting before the first buzz.
    pub break_after: u64,
    /// No input for this long means the user left the desk.
    pub idle_away: u64,
    /// A lock lasting this long counts as a break.
    pub lock_break: u64,
    /// Gap between repeat buzzes while the user keeps sitting.
    pub realert_every: u64,
    /// Buzzes per sitting session, first one included.
    pub max_alerts: u32,
    /// A gap between ticks longer than this means the PC slept.
    pub suspend_gap: u64,
    /// Post-call quiet window before a held buzz may fire.
    pub call_grace: u64,
    /// Longest a continuous call can hold a due buzz.
    pub hold_max: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            break_after: 45 * 60,
            idle_away: 5 * 60,
            lock_break: 2 * 60,
            realert_every: 10 * 60,
            max_alerts: 3,
            suspend_gap: 60,
            call_grace: 30,
            hold_max: 45 * 60,
        }
    }
}

/// Returns whether a microphone-capturing process name counts as an active call.
///
/// * `app` - Executable file name from the audio session (e.g. `"Teams.exe"`).
pub fn is_call_app(app: &str) -> bool {
    let trimmed = app.trim();
    !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case("pid:0")
        && !IGNORED_MIC_APPS.iter().any(|ignored| trimmed.eq_ignore_ascii_case(ignored))
}

/// Returns the first non-ignored microphone capture app, if any.
///
/// * `apps` - Active capture executable names reported by the OS.
pub fn active_call_app(apps: &[String]) -> Option<&str> {
    apps.iter().map(String::as_str).find(|app| is_call_app(app))
}

/// A buzz the caller must deliver.
#[derive(Debug, PartialEq, Eq)]
pub struct Alert {
    /// Whole minutes of continuous sitting.
    pub minutes: u64,
    /// 1 for the first buzz of a session, then 2, 3.
    pub number: u32,
}

/// Tracks one sitting session from periodic samples.
#[derive(Debug, Default)]
pub struct Tracker {
    sitting_since: Option<u64>,
    locked_since: Option<u64>,
    last_tick: Option<u64>,
    last_alert: Option<u64>,
    alerts_sent: u32,
    last_call: Option<u64>,
    held_since: Option<u64>,
}

impl Tracker {
    /// Forgets the current session (used on pause, sleep and real breaks).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whole seconds sat in the current session, 0 when away.
    ///
    /// * `now` - Monotonic or wall-clock timestamp in seconds.
    pub fn seconds(&self, now: u64) -> u64 {
        self.sitting_since.map_or(0, |since| now.saturating_sub(since))
    }

    /// Whole minutes sat in the current session, 0 when away.
    ///
    /// * `now` - Monotonic or wall-clock timestamp in seconds.
    pub fn minutes(&self, now: u64) -> u64 {
        self.seconds(now) / 60
    }

    /// Feeds one sample and returns a buzz when one is due and not held by a call.
    ///
    /// * `cfg` - Timing configuration.
    /// * `now` - Current timestamp in seconds.
    /// * `idle_secs` - Seconds since the last keyboard or mouse input.
    /// * `locked` - Whether the Windows session is locked.
    /// * `in_call` - Whether a non-ignored app is capturing from the microphone.
    pub fn tick(&mut self, cfg: &Config, now: u64, idle_secs: u64, locked: bool, in_call: bool) -> Option<Alert> {
        let slept = self.last_tick.is_some_and(|last| now.saturating_sub(last) > cfg.suspend_gap);
        self.last_tick = Some(now);
        if slept {
            self.reset();
            self.last_tick = Some(now);
        }

        self.locked_since = if locked { self.locked_since.or(Some(now)) } else { None };
        let lock_break = self.locked_since.is_some_and(|since| now - since >= cfg.lock_break);
        if lock_break || idle_secs >= cfg.idle_away {
            let locked_since = self.locked_since;
            self.reset();
            self.last_tick = Some(now);
            self.locked_since = locked_since;
            return None;
        }
        if locked {
            return None;
        }

        if in_call {
            self.last_call = Some(now);
        }
        let since = *self.sitting_since.get_or_insert(now);
        let sat = now - since;
        let due = match self.last_alert {
            None => sat >= cfg.break_after,
            Some(last) => self.alerts_sent < cfg.max_alerts && now - last >= cfg.realert_every,
        };
        if !due {
            return None;
        }

        let in_grace = self.last_call.is_some_and(|last| now.saturating_sub(last) < cfg.call_grace);
        if in_call || in_grace {
            let held_since = *self.held_since.get_or_insert(now);
            if now.saturating_sub(held_since) < cfg.hold_max {
                return None;
            }
        } else {
            self.held_since = None;
        }

        self.last_alert = Some(now);
        self.alerts_sent += 1;
        Some(Alert { minutes: sat / 60, number: self.alerts_sent })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEP: u64 = 5;

    /// Runs active (idle 0, unlocked, not in call) ticks from `from` to `to` inclusive, collecting buzz times.
    fn sit(t: &mut Tracker, cfg: &Config, from: u64, to: u64) -> Vec<(u64, u32)> {
        (from..=to)
            .step_by(STEP as usize)
            .filter_map(|now| t.tick(cfg, now, 0, false, false).map(|a| (now, a.number)))
            .collect()
    }

    #[test]
    fn buzzes_at_45_minutes_then_every_10_up_to_three() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        let buzzes = sit(&mut t, &cfg, 0, 3 * 3600);
        assert_eq!(buzzes, vec![(2700, 1), (3300, 2), (3900, 3)]);
    }

    #[test]
    fn no_buzz_before_the_limit() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        assert!(sit(&mut t, &cfg, 0, 2695).is_empty());
        assert_eq!(t.minutes(2695), 44);
    }

    #[test]
    fn five_minutes_idle_is_a_break() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        assert!(sit(&mut t, &cfg, 0, 2400).is_empty());
        assert_eq!(t.tick(&cfg, 2700, 300, false, false), None);
        assert_eq!(t.minutes(2700), 0);
        // A fresh session starts counting from the return.
        assert_eq!(sit(&mut t, &cfg, 2705, 2705 + 2700), vec![(2705 + 2700, 1)]);
    }

    #[test]
    fn short_idle_keeps_the_session() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2000);
        for (now, idle) in (2005..=2290).step_by(5).zip((5..).step_by(5)) {
            assert_eq!(t.tick(&cfg, now, idle, false, false), None);
        }
        assert_eq!(sit(&mut t, &cfg, 2295, 2700), vec![(2700, 1)]);
    }

    #[test]
    fn two_minute_lock_is_a_break_but_a_short_lock_is_not() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 1000);
        for now in (1005..=1100).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, true, false), None);
        }
        assert_eq!(t.minutes(1100), 18, "a 95 s lock must not reset");
        for now in (1105..=1300).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, true, false), None);
        }
        assert_eq!(t.minutes(1300), 0, "a lock of 2 min or more resets");
    }

    #[test]
    fn never_buzzes_while_locked() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2690);
        assert_eq!(t.tick(&cfg, 2700, 0, true, false), None);
    }

    #[test]
    fn a_gap_between_ticks_means_sleep_and_resets() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2600);
        assert_eq!(t.tick(&cfg, 2600 + 61, 0, false, false), None);
        assert_eq!(t.minutes(2600 + 61), 0);
    }

    #[test]
    fn a_break_after_a_buzz_allows_a_new_first_buzz() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        assert_eq!(sit(&mut t, &cfg, 0, 2700), vec![(2700, 1)]);
        t.tick(&cfg, 2705, 300, false, false);
        assert_eq!(sit(&mut t, &cfg, 2710, 2710 + 2700), vec![(2710 + 2700, 1)]);
    }

    #[test]
    fn holds_buzz_during_call_and_fires_after_30s_grace() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2600);
        for now in (2605..=2800).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, true), None, "must hold while call is active at {now}");
        }
        for now in (2805..=2825).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, false), None, "must hold during 30s call_grace at {now}");
        }
        assert_eq!(t.tick(&cfg, 2830, 0, false, false), Some(Alert { minutes: 47, number: 1 }));
    }

    #[test]
    fn call_ending_just_before_break_after_still_gets_full_grace() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2650);
        for now in (2655..=2685).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, true), None);
        }
        for now in (2690..=2710).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, false), None);
        }
        assert_eq!(t.tick(&cfg, 2715, 0, false, false), Some(Alert { minutes: 45, number: 1 }));
    }

    #[test]
    fn call_hold_caps_at_45_minutes_and_allows_realerts() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2695);
        for now in (2700..=5395).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, true), None, "must hold up to hold_max at {now}");
        }
        assert_eq!(t.tick(&cfg, 5400, 0, false, true), Some(Alert { minutes: 90, number: 1 }));
        for now in (5405..=5995).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, true), None);
        }
        assert_eq!(t.tick(&cfg, 6000, 0, false, true), Some(Alert { minutes: 100, number: 2 }));
    }

    #[test]
    fn filters_dictation_and_system_mic_apps_from_calls() {
        let ignored = vec!["Wispr Flow.exe".to_string(), "VoiceAccess.exe".to_string(), "audiodg.exe".to_string()];
        assert_eq!(active_call_app(&ignored), None);

        let with_call = vec!["Wispr Flow.exe".to_string(), "ms-teams.exe".to_string()];
        assert_eq!(active_call_app(&with_call), Some("ms-teams.exe"));
        assert!(is_call_app("Zoom.exe"));
        assert!(is_call_app("chrome.exe"));
    }
}
