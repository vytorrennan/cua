use super::*;

fn native(id: u64, pid: u32, parent: Option<u64>, kind: &str) -> crate::x11::WindowInfo {
    crate::x11::WindowInfo {
        xid: id,
        pid: Some(pid),
        title: format!("window {id}"),
        transient_for: parent,
        modal: Some(kind == "dialog"),
        window_type: Some(kind.into()),
        is_on_screen: true,
        width: 400,
        height: 200,
        ..Default::default()
    }
}

#[test]
fn native_metadata_survives_public_window_records() {
    let child = native(2, 8, Some(1), "dialog");
    let record = window_record_json(&child);
    assert_eq!(record["transient_for"], 1);
    assert_eq!(record["modal"], true);
    assert_eq!(record["window_type"], "dialog");
    assert_eq!(record["popup"], false);
}

#[test]
fn native_overlays_follow_parent_chains_including_cross_process_and_nested_popups() {
    let windows = [
        native(1, 7, None, "normal"),
        native(2, 8, Some(1), "dialog"),
        native(3, 8, Some(2), "popup"),
        native(4, 7, Some(99), "dialog"),
    ];
    let out = wayland_window_overlays(&windows, 7, 1);
    assert_eq!(out.dialogs.len(), 1);
    assert_eq!(out.dialogs[0]["window_id"], 2);
    assert_eq!(out.popups.len(), 1);
    assert_eq!(out.popups[0]["window_id"], 3);
    assert_eq!(out.popups[0]["transient_for"], 2);
    let hint = out.follow_up(7).unwrap();
    assert!(hint.contains("pid=8, window_id=2"));
    assert!(hint.contains("pid=8, window_id=3"));
    assert!(!hint.contains("pid=7, window_id=2"));
}

#[test]
fn visibility_and_native_parentage_prevent_unrelated_overlay_claims() {
    let mut hidden = native(3, 7, Some(1), "popup");
    hidden.is_on_screen = false;
    let windows = [
        native(1, 7, None, "normal"),
        native(2, 7, None, "dialog"),
        hidden,
    ];
    let out = wayland_window_overlays(&windows, 7, 1);
    assert!(out.dialogs.is_empty());
    assert!(out.popups.is_empty());
    assert!(!out.covers_window);
}

#[test]
fn old_providers_do_not_publish_invented_modality_or_role() {
    let window = crate::x11::WindowInfo {
        xid: 1,
        ..Default::default()
    };
    let record = window_record_json(&window);
    assert!(record.get("transient_for").is_none());
    assert!(record.get("modal").is_none());
    assert!(record.get("window_type").is_none());
}

#[test]
fn pid_only_observation_selects_visible_native_popup_then_dialog() {
    let mut parent = native(1, 7, None, "normal");
    parent.z_index = Some(10);
    let mut dialog = native(2, 7, Some(1), "dialog");
    dialog.z_index = Some(11);
    let mut popup = native(3, 7, Some(2), "popup");
    popup.z_index = Some(12);
    let unrelated = native(4, 8, None, "popup");
    assert_eq!(
        wayland_observation_window(
            &[parent.clone(), dialog.clone(), popup.clone(), unrelated],
            7
        ),
        Some(3)
    );
    popup.is_on_screen = false;
    assert_eq!(
        wayland_observation_window(&[parent, dialog, popup], 7),
        Some(2)
    );
}
