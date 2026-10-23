//! Relationships attested by a compositor, never inferred from PID or overlap.
use crate::x11::WindowInfo;
use std::collections::HashSet;

pub(crate) fn is_descendant(windows: &[WindowInfo], child: u64, ancestor: u64) -> bool {
    if child == ancestor {
        return false;
    }
    let mut seen = HashSet::new();
    let mut current = child;
    while seen.insert(current) {
        let Some(parent) = windows
            .iter()
            .find(|w| w.xid == current)
            .and_then(|w| w.transient_for)
        else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        current = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window(id: u64, parent: Option<u64>, pid: u32) -> WindowInfo {
        WindowInfo {
            xid: id,
            transient_for: parent,
            pid: Some(pid),
            ..Default::default()
        }
    }
    #[test]
    fn nested_and_cross_process_children_belong_only_to_their_native_parent() {
        let windows = [
            window(1, None, 7),
            window(2, None, 7),
            window(3, Some(1), 8),
            window(4, Some(3), 8),
        ];
        assert!(is_descendant(&windows, 3, 1));
        assert!(is_descendant(&windows, 4, 1));
        assert!(!is_descendant(&windows, 4, 2));
        assert!(!is_descendant(&windows, 1, 1));
    }
    #[test]
    fn missing_parents_and_cycles_do_not_invent_a_relationship() {
        let windows = [
            window(1, Some(2), 7),
            window(2, Some(1), 7),
            window(3, Some(99), 7),
        ];
        assert!(!is_descendant(&windows, 1, 4));
        assert!(!is_descendant(&windows, 3, 4));
    }
}
