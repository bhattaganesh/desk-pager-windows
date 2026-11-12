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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Continuous sitting before a quick stretch buzz (Tier 1).
    pub break_after: u64,
    /// Brief idle pause after a quick stretch buzz that resets the short cycle (e.g. 30 s).
    pub short_break_secs: u64,
    /// Cumulative sitting across quick stretches before a long break buzz (Tier 2).
    pub long_break_after: u64,
    /// No input for this long means the user took a full long break away from the desk.
    pub idle_away: u64,
    /// A lock lasting this long counts as a full break.
    pub lock_break: u64,
    /// Gap between repeat buzzes while the user keeps sitting.
    pub realert_every: u64,
    /// Buzzes per sitting cycle, first one included.
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
            short_break_secs: 30,
            long_break_after: 120 * 60,
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

/// Which tier of break reminder triggered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    /// Short 20-60s pause to stand up, stretch the lower back, rest eyes, and drink water.
    QuickStretch,
    /// Longer 5-15m recovery break away from the desk after extended sitting (e.g. 2 hours).
    LongBreak,
}

/// A buzz the caller must deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Alert {
    /// Which break tier is due.
    pub kind: AlertKind,
    /// Whole minutes sat in the relevant tier (`short` for QuickStretch, `long` for LongBreak).
    pub minutes: u64,
    /// 1 for the first buzz of the cycle, then 2, 3.
    pub number: u32,
}

/// Tracks both the quick-stretch cycle and cumulative long-break session from periodic samples.
#[derive(Debug, Default)]
pub struct Tracker {
    sitting_since: Option<u64>,
    long_sitting_since: Option<u64>,
    locked_since: Option<u64>,
    last_tick: Option<u64>,
    last_alert: Option<u64>,
    alerts_sent: u32,
    long_last_alert: Option<u64>,
    long_alerts_sent: u32,
    stretches_completed: u32,
    last_call: Option<u64>,
    held_since: Option<u64>,
}

impl Tracker {
    /// Forgets the entire session (used on pause, sleep, and full long breaks).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Resets only the quick-stretch cycle while preserving cumulative long-break sitting time.
    pub fn reset_short(&mut self) {
        self.sitting_since = None;
        self.last_alert = None;
        self.alerts_sent = 0;
        self.held_since = None;
        self.stretches_completed = self.stretches_completed.saturating_add(1);
    }

    /// Number of quick stretches completed within the current long-break session.
    pub fn stretches_completed(&self) -> u32 {
        self.stretches_completed
    }

    /// Whole seconds sat in the current quick-stretch cycle, 0 when away.
    ///
    /// * `now` - Monotonic or wall-clock timestamp in seconds.
    pub fn seconds(&self, now: u64) -> u64 {
        self.sitting_since.map_or(0, |since| now.saturating_sub(since))
    }

    /// Whole minutes sat in the current quick-stretch cycle, 0 when away.
    ///
    /// * `now` - Monotonic or wall-clock timestamp in seconds.
    pub fn minutes(&self, now: u64) -> u64 {
        self.seconds(now) / 60
    }

    /// Cumulative seconds sat toward the long break across quick stretches, 0 when away.
    ///
    /// * `now` - Monotonic or wall-clock timestamp in seconds.
    pub fn long_seconds(&self, now: u64) -> u64 {
        self.long_sitting_since.map_or(0, |since| now.saturating_sub(since))
    }

    /// Cumulative whole minutes sat toward the long break across quick stretches, 0 when away.
    ///
    /// * `now` - Monotonic or wall-clock timestamp in seconds.
    pub fn long_minutes(&self, now: u64) -> u64 {
        self.long_seconds(now) / 60
    }

    /// Feeds one sample and returns a buzz when a quick stretch or long break is due and not held by a call.
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

        // If a quick-stretch reminder has already fired, an idle pause of `short_break_secs`
        // (standing up to stretch, rest eyes, drink water) resets the quick-stretch cycle only.
        if self.alerts_sent > 0 && idle_secs >= cfg.short_break_secs {
            self.reset_short();
            return None;
        }

        if in_call {
            self.last_call = Some(now);
        }

        // Do not start a new sitting cycle while the user is still idle/away after a break.
        let active_now = in_call || idle_secs < cfg.short_break_secs;
        if self.long_sitting_since.is_none() && !active_now {
            return None;
        }
        let long_since = *self.long_sitting_since.get_or_insert(now);
        let long_sat = now - long_since;

        if self.sitting_since.is_none() && !active_now {
            return None;
        }
        let short_since = *self.sitting_since.get_or_insert(now);
        let short_sat = now - short_since;

        let long_enabled = cfg.long_break_after > 0;
        let long_due = long_enabled
            && match self.long_last_alert {
                None => long_sat >= cfg.long_break_after,
                Some(last) => self.long_alerts_sent < cfg.max_alerts && now - last >= cfg.realert_every,
            };

        let short_due = (!long_enabled || long_sat < cfg.long_break_after)
            && match self.last_alert {
                None => short_sat >= cfg.break_after,
                Some(last) => self.alerts_sent < cfg.max_alerts && now - last >= cfg.realert_every,
            };

        if !long_due && !short_due {
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

        if long_due {
            self.long_last_alert = Some(now);
            self.long_alerts_sent += 1;
            Some(Alert {
                kind: AlertKind::LongBreak,
                minutes: long_sat / 60,
                number: self.long_alerts_sent,
            })
        } else {
            self.last_alert = Some(now);
            self.alerts_sent += 1;
            Some(Alert {
                kind: AlertKind::QuickStretch,
                minutes: short_sat / 60,
                number: self.alerts_sent,
            })
        }
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
    fn buzzes_at_45_minutes_then_every_10_up_to_three_and_long_break_at_2h() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        let buzzes: Vec<(u64, AlertKind, u32)> = (0..=3 * 3600)
            .step_by(STEP as usize)
            .filter_map(|now| t.tick(&cfg, now, 0, false, false).map(|a| (now, a.kind, a.number)))
            .collect();
        assert_eq!(
            buzzes,
            vec![
                (2700, AlertKind::QuickStretch, 1),
                (3300, AlertKind::QuickStretch, 2),
                (3900, AlertKind::QuickStretch, 3),
                (7200, AlertKind::LongBreak, 1),
                (7800, AlertKind::LongBreak, 2),
                (8400, AlertKind::LongBreak, 3),
            ]
        );
    }

    #[test]
    fn quick_stretch_resets_short_cycle_while_long_break_accumulates_to_2_hours() {
        let cfg = Config {
            break_after: 30 * 60,
            short_break_secs: 30,
            long_break_after: 120 * 60,
            idle_away: 10 * 60,
            ..Config::default()
        };
        let mut t = Tracker::default();

        // Sit for 30 min -> QuickStretch #1 at 1800s.
        assert_eq!(
            sit(&mut t, &cfg, 0, 1800),
            vec![(1800, 1)]
        );
        assert_eq!(t.minutes(1800), 30);
        assert_eq!(t.long_minutes(1800), 30);

        // Stand up and stretch for 30s (1805..=1830).
        for (now, idle) in (1805..=1830).step_by(5).zip((5..=30).step_by(5)) {
            assert_eq!(t.tick(&cfg, now, idle, false, false), None);
        }
        // Short stretch counter reset to 0, but cumulative long-break counter is still 30 min!
        assert_eq!(t.minutes(1830), 0);
        assert_eq!(t.long_minutes(1830), 30);

        // Sit another 30 min from 1835..=3635 -> QuickStretch #1 again at 3635s.
        assert_eq!(sit(&mut t, &cfg, 1835, 3635), vec![(3635, 1)]);
        assert_eq!(t.minutes(3635), 30);
        assert_eq!(t.long_minutes(3635), 60);

        // Take a 30s stretch again (3640..=3665).
        for (now, idle) in (3640..=3665).step_by(5).zip((5..=30).step_by(5)) {
            assert_eq!(t.tick(&cfg, now, idle, false, false), None);
        }
        assert_eq!(t.minutes(3665), 0);
        assert_eq!(t.long_minutes(3665), 61);

        // Sit another 30 min from 3670..=5470 -> QuickStretch #1 again at 5470s.
        assert_eq!(sit(&mut t, &cfg, 3670, 5470), vec![(5470, 1)]);
        for (now, idle) in (5475..=5500).step_by(5).zip((5..=30).step_by(5)) {
            assert_eq!(t.tick(&cfg, now, idle, false, false), None);
        }
        assert_eq!(t.minutes(5500), 0);
        assert_eq!(t.long_minutes(5500), 91);

        // Keep working until 7200s (2 hours cumulative) -> LongBreak #1 fires at 7200s!
        let mut long_alerts = Vec::new();
        for now in (5505..=7200).step_by(5) {
            if let Some(a) = t.tick(&cfg, now, 0, false, false) {
                long_alerts.push((now, a));
            }
        }
        assert_eq!(
            long_alerts,
            vec![(
                7200,
                Alert {
                    kind: AlertKind::LongBreak,
                    minutes: 120,
                    number: 1,
                }
            )]
        );

        // A 10-minute walk break (idle_secs = 600) resets both short and long counters.
        assert_eq!(t.tick(&cfg, 7205, 600, false, false), None);
        assert_eq!(t.minutes(7205), 0);
        assert_eq!(t.long_minutes(7205), 0);
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
        assert_eq!(
            t.tick(&cfg, 2830, 0, false, false),
            Some(Alert { kind: AlertKind::QuickStretch, minutes: 47, number: 1 })
        );
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
        assert_eq!(
            t.tick(&cfg, 2715, 0, false, false),
            Some(Alert { kind: AlertKind::QuickStretch, minutes: 45, number: 1 })
        );
    }

    #[test]
    fn call_hold_caps_at_45_minutes_and_allows_realerts() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2695);
        for now in (2700..=5395).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, true), None, "must hold up to hold_max at {now}");
        }
        assert_eq!(
            t.tick(&cfg, 5400, 0, false, true),
            Some(Alert { kind: AlertKind::QuickStretch, minutes: 90, number: 1 })
        );
        for now in (5405..=5995).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, false, true), None);
        }
        assert_eq!(
            t.tick(&cfg, 6000, 0, false, true),
            Some(Alert { kind: AlertKind::QuickStretch, minutes: 100, number: 2 })
        );
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
