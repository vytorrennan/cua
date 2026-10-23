//! Proven joins between KWin's lifetime UUIDs and AT-SPI object references.
//! Titles, dimensions, window tokens and child ordinals may all change or tie.

use std::collections::HashMap;

type Frame = (String, String);

#[derive(Default)]
pub(super) struct WindowBindings {
    entries: HashMap<(u32, String), Frame>,
}

impl WindowBindings {
    pub(super) fn remember(&mut self, pid: u32, handle: &str, frame: Frame) {
        // A proved reassignment supersedes both sides of an earlier join.
        self.entries.retain(|(owner, native), accessible| {
            *owner != pid || native == handle || *accessible != frame
        });
        if self.entries.len() >= 4096 {
            self.entries.clear();
        }
        self.entries.insert((pid, handle.to_owned()), frame);
    }

    pub(super) fn resolve(&self, pid: u32, handle: &str, frames: &[Frame]) -> Option<usize> {
        let known = self.entries.get(&(pid, handle.to_owned()))?;
        frames.iter().position(|frame| frame == known)
    }

    pub(super) fn retain_live(&mut self, windows: &[(u32, String)], pid: u32, frames: &[Frame]) {
        self.entries
            .retain(|key, frame| windows.contains(key) && (key.0 != pid || frames.contains(frame)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(path: &str) -> Frame {
        (":1.42".into(), path.into())
    }

    #[test]
    fn identical_windows_stay_bound_when_focus_titles_and_seed_order_change() {
        let mut bindings = WindowBindings::default();
        bindings.remember(7, "uuid-alpha", frame("alpha"));
        bindings.remember(7, "uuid-beta", frame("beta"));
        assert_eq!(
            bindings.resolve(7, "uuid-alpha", &[frame("beta"), frame("alpha")]),
            Some(1)
        );
        assert_eq!(
            bindings.resolve(7, "uuid-beta", &[frame("beta"), frame("alpha")]),
            Some(0)
        );
        assert_eq!(bindings.resolve(8, "uuid-alpha", &[frame("alpha")]), None);
        assert_eq!(
            bindings.resolve(7, "new-window-reusing-token", &[frame("alpha")]),
            None
        );
    }

    #[test]
    fn closed_windows_and_restarted_accessibility_bridges_cannot_reuse_a_binding() {
        let mut bindings = WindowBindings::default();
        bindings.remember(7, "uuid-alpha", frame("alpha"));
        bindings.retain_live(&[], 7, &[frame("alpha")]);
        assert_eq!(bindings.resolve(7, "uuid-alpha", &[frame("alpha")]), None);
        bindings.remember(7, "uuid-alpha", frame("alpha"));
        let restarted = (":1.99".into(), "alpha".into());
        bindings.retain_live(&[(7, "uuid-alpha".into())], 7, &[restarted.clone()]);
        assert_eq!(bindings.resolve(7, "uuid-alpha", &[restarted]), None);
    }

    #[test]
    fn a_new_observation_replaces_conflicting_joins_without_affecting_another_app() {
        let mut bindings = WindowBindings::default();
        bindings.remember(7, "alpha", frame("one"));
        bindings.remember(8, "other-app", frame("one"));
        bindings.remember(7, "beta", frame("one"));
        assert_eq!(bindings.resolve(7, "alpha", &[frame("one")]), None);
        assert_eq!(bindings.resolve(7, "beta", &[frame("one")]), Some(0));
        assert_eq!(bindings.resolve(8, "other-app", &[frame("one")]), Some(0));
    }
}
