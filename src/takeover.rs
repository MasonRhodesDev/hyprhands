//! Whether the owner has taken the seat back, judged from what Hyprland's IPC shows.
//!
//! The first reason latches; every input after it is refused until the next session's
//! `baseline`. Observations while hyprhands' own input runs, and for `SETTLE` after it, are its
//! own echo (the pointer it moved, the focus it changed) and never count.
//!
//! - The cursor more than `tolerance` px from where hyprhands last left it: a hand on the mouse.
//! - The focused monitor leaving the driven one: a click or workspace switch elsewhere.
//!
//! Keystrokes are not among the signals: Hyprland's IPC never streams them, and hyprhands never
//! changes the owner's config (a bind that reported keys would be exactly that). A keyboard-only
//! takeover still shows once it moves focus to another monitor.

use std::time::{Duration, Instant};

pub const SETTLE: Duration = Duration::from_millis(400);

pub struct Takeover {
    tolerance: f64,
    expected: Option<(f64, f64)>,
    busy: u32,
    quiet_until: Option<Instant>,
    reason: Option<String>,
}

impl Takeover {
    pub fn new(tolerance: f64) -> Self {
        Self { tolerance, expected: None, busy: 0, quiet_until: None, reason: None }
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    fn echo(&self, now: Instant) -> bool {
        self.busy > 0 || self.quiet_until.is_some_and(|q| now < q)
    }

    fn latch(&mut self, reason: String) {
        self.reason.get_or_insert(reason);
    }

    pub fn baseline(&mut self, cursor: (f64, f64)) {
        *self = Self { tolerance: self.tolerance, expected: Some(cursor), ..Self::new(self.tolerance) };
    }

    pub fn begin(&mut self) {
        self.busy += 1;
    }

    /// Input finished; `cursor` is where it left the pointer (None if it could not be read).
    pub fn end(&mut self, cursor: Option<(f64, f64)>, now: Instant) {
        self.busy = self.busy.saturating_sub(1);
        if cursor.is_some() {
            self.expected = cursor;
        }
        self.quiet_until = Some(now + SETTLE);
    }

    pub fn cursor_seen(&mut self, pos: (f64, f64), now: Instant) {
        let Some(exp) = self.expected else { return };
        if self.echo(now) {
            return;
        }
        let drift = (pos.0 - exp.0).hypot(pos.1 - exp.1);
        if drift > self.tolerance {
            self.latch(format!("owner took over: mouse moved {drift:.0}px from where hyprhands left it"));
        }
    }

    pub fn focused_monitor(&mut self, name: &str, driven: &str, now: Instant) {
        if name != driven && !self.echo(now) {
            self.latch(format!("owner took over: focus moved to {name}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat() -> (Takeover, Instant) {
        let mut t = Takeover::new(64.0);
        t.baseline((500.0, 500.0));
        (t, Instant::now())
    }

    #[test]
    fn small_drift_is_still_ours_a_hand_on_the_mouse_latches() {
        let (mut t, now) = seat();
        t.cursor_seen((540.0, 530.0), now);
        assert!(t.reason().is_none());
        t.cursor_seen((900.0, 500.0), now);
        assert!(t.reason().unwrap().contains("400px"));
        t.cursor_seen((500.0, 500.0), now);
        assert!(t.reason().is_some(), "the first reason sticks");
    }

    #[test]
    fn our_own_moves_and_their_settle_never_count() {
        let (mut t, now) = seat();
        t.begin();
        t.cursor_seen((1500.0, 900.0), now);
        t.end(Some((1500.0, 900.0)), now);
        t.cursor_seen((1520.0, 910.0), now + Duration::from_millis(100));
        let later = now + SETTLE + Duration::from_millis(100);
        t.cursor_seen((1510.0, 905.0), later);
        assert!(t.reason().is_none());
        t.cursor_seen((1500.0, 1100.0), later);
        assert!(t.reason().is_some());
    }

    #[test]
    fn focus_leaving_the_driven_monitor_latches() {
        let (mut t, now) = seat();
        t.focused_monitor("HDMI-A-1", "HDMI-A-1", now);
        assert!(t.reason().is_none());
        t.focused_monitor("DP-2", "HDMI-A-1", now);
        assert_eq!(t.reason(), Some("owner took over: focus moved to DP-2"));
    }

    #[test]
    fn a_new_baseline_starts_clean() {
        let (mut t, now) = seat();
        t.cursor_seen((0.0, 0.0), now);
        t.baseline((10.0, 10.0));
        assert!(t.reason().is_none());
    }
}
