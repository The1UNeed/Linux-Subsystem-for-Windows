//! Idle lifecycle: pause the vCPUs (0 % host CPU) when no Windows window is
//! visible, resume as soon as one is wanted again.

use std::time::{Duration, Instant};

/// Pause after this long with nothing visible.
pub const IDLE_AFTER: Duration = Duration::from_secs(60);

/// What the supervisor observes every few seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observation {
    /// The Window/Full Screen view is attached (its QMP monitor is
    /// connected; it shows the display whether or not the agent runs).
    pub window_view: bool,
    /// The Coherence view is attached (Coherence port connected).
    pub coherence_view: bool,
    /// Windows mirrored by Coherence (as it last reported).
    pub coherence_windows: u32,
    /// The guest agent is connected. Until it is, Windows is booting,
    /// installing updates or rebooting, and is never paused: a frozen
    /// update or logon would never finish.
    pub agent: bool,
}

impl Observation {
    /// Whether anything of Windows is on screen.
    pub fn visible(&self) -> bool {
        self.window_view || (self.coherence_view && self.coherence_windows > 0)
    }
}

/// What to do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Pause,
    Resume,
}

#[derive(Debug, Default)]
pub struct IdlePolicy {
    idle_since: Option<Instant>,
    paused: bool,
}

impl IdlePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Something needs Windows right now (e.g. a launcher): resume.
    pub fn wake(&mut self, now: Instant) -> Action {
        self.idle_since = None;
        if self.paused {
            self.paused = false;
            let _ = now;
            Action::Resume
        } else {
            Action::None
        }
    }

    pub fn observe(&mut self, obs: Observation, now: Instant) -> Action {
        if obs.visible() {
            return self.wake(now);
        }
        if self.paused {
            return Action::None;
        }
        if !obs.agent {
            self.idle_since = None;
            return Action::None;
        }
        let since = *self.idle_since.get_or_insert(now);
        if now.duration_since(since) >= IDLE_AFTER {
            self.paused = true;
            Action::Pause
        } else {
            Action::None
        }
    }
}

/// Whether a `query-chardev` filename shows a connected client.
pub fn chardev_connected(filename: &str) -> bool {
    !filename.starts_with("disconnected:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    #[test]
    fn pauses_after_a_minute_of_nothing_visible_and_resumes_on_demand() {
        let t0 = Instant::now();
        let mut p = IdlePolicy::new();
        let none = Observation {
            agent: true,
            ..Default::default()
        };
        assert_eq!(p.observe(none, at(t0, 200)), Action::None);
        assert_eq!(p.observe(none, at(t0, 259)), Action::None);
        assert_eq!(p.observe(none, at(t0, 260)), Action::Pause);
        assert!(p.paused());
        assert_eq!(p.observe(none, at(t0, 300)), Action::None, "already paused");
        let window = Observation {
            window_view: true,
            ..none
        };
        assert_eq!(p.observe(window, at(t0, 301)), Action::Resume);
        assert_eq!(p.observe(window, at(t0, 400)), Action::None);
    }

    #[test]
    fn coherence_with_windows_keeps_running_and_without_them_idles() {
        let t0 = Instant::now();
        let mut p = IdlePolicy::new();
        let busy = Observation {
            coherence_view: true,
            coherence_windows: 2,
            agent: true,
            ..Default::default()
        };
        let empty = Observation {
            coherence_windows: 0,
            ..busy
        };
        assert_eq!(p.observe(busy, at(t0, 500)), Action::None);
        assert_eq!(p.observe(empty, at(t0, 510)), Action::None);
        assert_eq!(p.observe(busy, at(t0, 540)), Action::None, "timer resets");
        assert_eq!(p.observe(empty, at(t0, 550)), Action::None);
        assert_eq!(p.observe(empty, at(t0, 611)), Action::Pause);
        // A launcher wakes it.
        assert_eq!(p.wake(at(t0, 620)), Action::Resume);
        assert_eq!(p.wake(at(t0, 621)), Action::None);
    }

    #[test]
    fn never_pauses_while_windows_boots_or_updates() {
        let t0 = Instant::now();
        let mut p = IdlePolicy::new();
        let booting = Observation::default();
        // A long update: no agent for 20 minutes, nothing on screen.
        for s in (0..1200).step_by(2) {
            assert_eq!(p.observe(booting, at(t0, s)), Action::None);
        }
        // The agent connects: the idle minute starts only now.
        let ready = Observation {
            agent: true,
            ..booting
        };
        assert_eq!(p.observe(ready, at(t0, 1200)), Action::None);
        assert_eq!(p.observe(ready, at(t0, 1259)), Action::None);
        // An in-guest reboot drops the agent: the timer starts again.
        assert_eq!(p.observe(booting, at(t0, 1250)), Action::None);
        assert_eq!(p.observe(ready, at(t0, 1300)), Action::None);
        assert_eq!(p.observe(ready, at(t0, 1359)), Action::None);
        assert_eq!(p.observe(ready, at(t0, 1360)), Action::Pause);
    }

    #[test]
    fn chardev_state() {
        assert!(!chardev_connected(
            "disconnected:unix:/run/x/session.sock,server=on"
        ));
        assert!(chardev_connected("unix:/run/x/session.sock,server=on"));
    }
}
