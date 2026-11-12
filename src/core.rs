//! The sitting timer: pure logic, no Windows calls, so it can be unit-tested.

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
        }
    }
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
}

impl Tracker {
    /// Forgets the current session (used on pause, sleep and real breaks).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Whole minutes sat in the current session, 0 when away.
    pub fn minutes(&self, now: u64) -> u64 {
        self.sitting_since.map_or(0, |since| now.saturating_sub(since) / 60)
    }

    /// Feeds one sample; returns a buzz when one is due.
    ///
    /// `now` is wall-clock seconds, `idle_secs` the time since the last keyboard or mouse input.
    pub fn tick(&mut self, cfg: &Config, now: u64, idle_secs: u64, locked: bool) -> Option<Alert> {
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

        let since = *self.sitting_since.get_or_insert(now);
        let sat = now - since;
        let due = match self.last_alert {
            None => sat >= cfg.break_after,
            Some(last) => self.alerts_sent < cfg.max_alerts && now - last >= cfg.realert_every,
        };
        if !due {
            return None;
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

    /// Runs active (idle 0, unlocked) ticks from `from` to `to` inclusive, collecting buzz times.
    fn sit(t: &mut Tracker, cfg: &Config, from: u64, to: u64) -> Vec<(u64, u32)> {
        (from..=to)
            .step_by(STEP as usize)
            .filter_map(|now| t.tick(cfg, now, 0, false).map(|a| (now, a.number)))
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
        assert_eq!(t.tick(&cfg, 2700, 300, false), None);
        assert_eq!(t.minutes(2700), 0);
        // A fresh session starts counting from the return.
        assert_eq!(sit(&mut t, &cfg, 2705, 2705 + 2700), vec![(2705 + 2700, 1)]);
    }

    #[test]
    fn short_idle_keeps_the_session() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2000);
        for (now, idle) in (2005..=2290).step_by(5).zip((5..).step_by(5)) {
            assert_eq!(t.tick(&cfg, now, idle, false), None);
        }
        assert_eq!(sit(&mut t, &cfg, 2295, 2700), vec![(2700, 1)]);
    }

    #[test]
    fn two_minute_lock_is_a_break_but_a_short_lock_is_not() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 1000);
        for now in (1005..=1100).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, true), None);
        }
        assert_eq!(t.minutes(1100), 18, "a 95 s lock must not reset");
        for now in (1105..=1300).step_by(5) {
            assert_eq!(t.tick(&cfg, now, 0, true), None);
        }
        assert_eq!(t.minutes(1300), 0, "a lock of 2 min or more resets");
    }

    #[test]
    fn never_buzzes_while_locked() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2690);
        assert_eq!(t.tick(&cfg, 2700, 0, true), None);
    }

    #[test]
    fn a_gap_between_ticks_means_sleep_and_resets() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        sit(&mut t, &cfg, 0, 2600);
        assert_eq!(t.tick(&cfg, 2600 + 61, 0, false), None);
        assert_eq!(t.minutes(2600 + 61), 0);
    }

    #[test]
    fn a_break_after_a_buzz_allows_a_new_first_buzz() {
        let (cfg, mut t) = (Config::default(), Tracker::default());
        assert_eq!(sit(&mut t, &cfg, 0, 2700), vec![(2700, 1)]);
        t.tick(&cfg, 2705, 300, false);
        assert_eq!(sit(&mut t, &cfg, 2710, 2710 + 2700), vec![(2710 + 2700, 1)]);
    }
}
