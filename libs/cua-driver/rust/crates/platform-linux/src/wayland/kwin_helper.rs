//! Trusted KWin window identity and target-bound input adapter.
//!
//! The optional in-process KWin effect exposes stable window tokens, process
//! identity, geometry, active/minimized state, and stacking metadata through a
//! D-Bus surface owned by `kwin_wayland` itself. Protocol v2 adds input
//! methods whose activation and injection both run inside the compositor:
//! each call resolves the token to a live window, activates that exact
//! window, verifies the result, and only then injects. There is no
//! check-then-inject window across process boundaries, so no other surface can
//! receive the event between the check and the delivery.
//!
//! Window discovery works with protocol v1. Activation and input require v2 and
//! are refused when only the read-only helper is present.

use std::collections::HashSet;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cua_driver_contract::ScrollBy;

use crate::x11::WindowInfo;

const DEST: &str = "org.cua.KWinTarget";
const KWIN_DEST: &str = "org.kde.KWin";
const PATH: &str = "/org/cua/KWinTarget";
const IFACE: &str = "org.cua.KWinTarget";
const DBUS_DEST: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_IFACE: &str = "org.freedesktop.DBus";
pub const PROTOCOL_VERSION: u32 = 2;
/// Protocol v3 adds target-bound background input (pointer and keyboard) plus
/// real-pointer position/warp. The effect must be v3 to use those routes.
pub const BACKGROUND_PROTOCOL_VERSION: u32 = 3;
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// `TypeText`/`TypeTextBackground` run the effect's key loop synchronously.
/// The effect inserts 10 ms after every key transition, so plain ASCII costs
/// about 20 ms/character and shifted ASCII about 40 ms. Keep the D-Bus wait
/// above the worst-case typing time instead of reporting a false failure after
/// three seconds while the compositor is still typing.
const TYPE_TEXT_MARGIN: Duration = Duration::from_secs(3);
const TYPE_TEXT_WORST_CASE_PER_CHAR: Duration = Duration::from_millis(40);
const TYPE_TEXT_TIMEOUT_MAX: Duration = Duration::from_secs(180);

thread_local! {
    /// Token of the KWin window whose foreground guard is active on this
    /// thread. Set only by [`with_focused_window`] for the duration of the
    /// body, so input wrappers can bind every primitive to that exact target.
    static ACTIVE_INPUT_TOKEN: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

struct InputTokenGuard(Option<u64>);

impl Drop for InputTokenGuard {
    fn drop(&mut self) {
        ACTIVE_INPUT_TOKEN.with(|token| token.set(self.0));
    }
}

/// Token bound by the innermost [`with_focused_window`] on this thread, if any.
pub fn active_input_token() -> Option<u64> {
    ACTIVE_INPUT_TOKEN.with(std::cell::Cell::get)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KwinWindow {
    pub token: u64,
    pub handle: String,
    pub pid: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub minimized: bool,
    pub stacking: usize,
    pub transient_for: Option<u64>,
    pub modal: Option<bool>,
    pub window_type: Option<String>,
}

/// Whether KWin currently offers the target-bound input protocol (v2).
///
/// A read-only v1 helper is still sufficient for discovery through
/// [`list_window_infos`], but it cannot authorize activation or input.
pub fn available() -> bool {
    helper_owner().is_some()
}

fn helper_json(owner: &str, method: &str, args: &[String]) -> Option<serde_json::Value> {
    let raw = call_to(owner, method, args)?;
    serde_json::from_str(&parse_quoted_string(&raw)?).ok()
}

/// Own one bounded focus calibration. Drop releases it even when the AT-SPI
/// future is cancelled by its caller's deadline. The effect also has a timer.
pub(crate) struct AccessibilityProbe {
    owner: String,
    lease: u64,
    finished: bool,
}

impl AccessibilityProbe {
    pub(crate) fn begin(pid: u32, token: u64) -> Option<Self> {
        let info = helper_info()?;
        if info.version < 4 || trusted_window_for_id(pid, token).is_none() {
            return None;
        }
        let response = helper_json(&info.owner, "BeginAccessibilityProbe", &[token.to_string()])?;
        if !response.get("ok")?.as_bool()? {
            return None;
        }
        Some(Self {
            owner: info.owner,
            lease: response.get("lease")?.as_u64()?,
            finished: false,
        })
    }

    pub(crate) fn finish(mut self) -> bool {
        self.finished = true;
        helper_json(
            &self.owner,
            "EndAccessibilityProbe",
            &[self.lease.to_string()],
        )
        .and_then(|response| response.get("ok").and_then(|v| v.as_bool()))
        .unwrap_or(false)
    }

    pub(crate) fn advance(&self) -> Option<()> {
        let response = helper_json(
            &self.owner,
            "AdvanceAccessibilityProbe",
            &[self.lease.to_string()],
        )?;
        if !response.get("ok")?.as_bool()? {
            return None;
        }
        Some(())
    }
}

impl Drop for AccessibilityProbe {
    fn drop(&mut self) {
        if !self.finished {
            let _ = helper_json(
                &self.owner,
                "EndAccessibilityProbe",
                &[self.lease.to_string()],
            );
        }
    }
}

pub fn list_windows() -> Option<Vec<KwinWindow>> {
    snapshot_for_owner(&helper_owner()?)
}

pub fn list_window_infos() -> Option<Vec<WindowInfo>> {
    Some(
        list_windows()?
            .into_iter()
            .map(|window| WindowInfo {
                transient_for: window.transient_for,
                modal: window.modal,
                window_type: window.window_type,
                xid: window.token,
                pid: Some(window.pid),
                app_name: window.app_name,
                title: window.title,
                is_on_screen: !window.minimized && window.width > 0 && window.height > 0,
                // KWin's stacking rank is bottom-to-top, higher is closer to
                // the front, matching the public z_index contract.
                z_index: Some(window.stacking),
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
            })
            .collect(),
    )
}

/// Whether the loaded effect can deliver target-bound background input (v3).
pub fn supports_background() -> bool {
    helper_info().is_some_and(|info| info.version >= BACKGROUND_PROTOCOL_VERSION)
}

/// The real compositor pointer position (global/output coordinates).
pub fn pointer_position() -> Option<(f64, f64)> {
    let owner = helper_owner()?;
    let raw = call_to(&owner, "GetPointer", &[])?;
    let payload = parse_quoted_string(&raw)?;
    let value: serde_json::Value = serde_json::from_str(&payload).ok()?;
    Some((value.get("x")?.as_f64()?, value.get("y")?.as_f64()?))
}

/// Warp the real compositor pointer to a global/output point.
pub fn move_pointer(x: f64, y: f64) -> anyhow::Result<()> {
    let owner = background_owner()?;
    input_call(
        &owner,
        "MovePointer",
        &[format!("{x:.3}"), format!("{y:.3}")],
    )
}

pub fn trusted_window_for_id(pid: u32, token: u64) -> Option<KwinWindow> {
    snapshot_for_owner(&helper_owner()?)?
        .into_iter()
        .find(|window| window.pid == pid && window.token == token)
}

/// Activate one exact KWin window and leave it active.
///
/// Used by tools whose contract is "make this window the active one" rather
/// than "borrow focus for one action" (`bring_to_front`, `invoke_menu`).
pub fn activate(pid: u32, token: u64) -> anyhow::Result<()> {
    let owner = helper_owner().ok_or_else(|| {
        anyhow::anyhow!(
            "foreground_unavailable: no trusted KWin target-input helper (protocol \
             v{PROTOCOL_VERSION}) is loaded"
        )
    })?;
    if trusted_window_for_id(pid, token).is_none() {
        anyhow::bail!(
            "foreground_unavailable: KWin window token {token} does not belong to pid {pid}"
        );
    }
    input_call(&owner, "Activate", &[token.to_string()]).map_err(|error| {
        anyhow::anyhow!("foreground_unavailable: KWin refused activation: {error}")
    })?;
    Ok(())
}

/// Run a foreground transaction bound to one exact KWin window.
///
/// The owner is re-verified, the target is confirmed to belong to `pid`, the
/// window is activated inside the compositor, and the token is bound for the
/// body so every input primitive addresses that same window. The previously
/// active window is restored afterwards.
pub fn with_focused_window<T>(
    pid: u32,
    token: u64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let owner = helper_owner().ok_or_else(|| {
        anyhow::anyhow!(
            "foreground_unavailable: no trusted KWin target-input helper (protocol \
             v{PROTOCOL_VERSION}) is loaded"
        )
    })?;
    let target = trusted_window_for_id(pid, token);
    if target.is_none() {
        anyhow::bail!(
            "foreground_unavailable: KWin window token {token} does not belong to pid {pid}"
        );
    }

    if target
        .is_some_and(|window| matches!(window.window_type.as_deref(), Some("popup" | "tooltip")))
    {
        let restore = ACTIVE_INPUT_TOKEN.with(|active| active.replace(Some(token)));
        let _guard = InputTokenGuard(restore);
        return body();
    }

    let previous = call_to(&owner, "GetActiveToken", &[]).and_then(|raw| parse_first_i64(&raw));
    input_call(&owner, "Activate", &[token.to_string()]).map_err(|error| {
        anyhow::anyhow!("foreground_unavailable: KWin refused activation: {error}")
    })?;

    let restore = ACTIVE_INPUT_TOKEN.with(|active| active.replace(Some(token)));
    let _guard = InputTokenGuard(restore);
    let result = body();

    if let Some(previous) = previous {
        if previous > 0 && previous != token as i64 {
            let _ = input_call(&owner, "Activate", &[previous.to_string()]);
        }
    }
    result
}

/// Move the cursor over a target-bound window without pressing a button.
pub fn move_to(token: u64, x: i32, y: i32) -> anyhow::Result<()> {
    let owner = input_owner()?;
    input_call(
        &owner,
        "Move",
        &[token.to_string(), x.to_string(), y.to_string()],
    )
}

/// Press and release a pointer button at an output-relative point without
/// raising or activating the target window. The pointer is restored afterwards.
pub fn click_background(token: u64, x: i32, y: i32, count: u32, button: u8) -> anyhow::Result<()> {
    let owner = background_owner()?;
    input_call(
        &owner,
        "ClickBackground",
        &[
            token.to_string(),
            x.to_string(),
            y.to_string(),
            evdev_button(button).to_string(),
            count.max(1).to_string(),
        ],
    )
}

/// Press and release a pointer button at an output-relative point.
pub fn click(token: u64, x: i32, y: i32, count: u32, button: u8) -> anyhow::Result<()> {
    let owner = input_owner()?;
    input_call(
        &owner,
        "Click",
        &[
            token.to_string(),
            x.to_string(),
            y.to_string(),
            evdev_button(button).to_string(),
            count.max(1).to_string(),
        ],
    )
}

/// KWin has one wheel-axis stream; use the same three-detent page
/// approximation as the Hyprland path rather than silently treating a page as
/// one line-sized tick.
fn scroll_ticks(amount: u32, by: ScrollBy) -> u32 {
    let amount = amount.max(1);
    match by {
        ScrollBy::Line => amount,
        ScrollBy::Page => amount.saturating_mul(3),
    }
}

/// Scroll `amount` wheel notches in `direction` with the pointer over `(x, y)`.
pub fn scroll(
    token: u64,
    x: i32,
    y: i32,
    direction: &str,
    amount: u32,
    by: ScrollBy,
) -> anyhow::Result<()> {
    let (dx, dy): (f64, f64) = match direction.to_ascii_lowercase().as_str() {
        "up" => (0.0, -1.0),
        "down" => (0.0, 1.0),
        "left" => (-1.0, 0.0),
        "right" => (1.0, 0.0),
        other => anyhow::bail!("unknown scroll direction: {other}"),
    };
    let amount = scroll_ticks(amount, by) as f64;
    let owner = input_owner()?;
    input_call(
        &owner,
        "Scroll",
        &[
            token.to_string(),
            x.to_string(),
            y.to_string(),
            format!("{:.3}", dx * amount),
            format!("{:.3}", dy * amount),
        ],
    )
}

/// Scroll without activating the target; the pointer is restored afterwards.
pub fn scroll_background(
    token: u64,
    x: i32,
    y: i32,
    direction: &str,
    amount: u32,
    by: ScrollBy,
) -> anyhow::Result<()> {
    let (dx, dy): (f64, f64) = match direction.to_ascii_lowercase().as_str() {
        "up" => (0.0, -1.0),
        "down" => (0.0, 1.0),
        "left" => (-1.0, 0.0),
        "right" => (1.0, 0.0),
        other => anyhow::bail!("unknown scroll direction: {other}"),
    };
    let amount = scroll_ticks(amount, by) as f64;
    let owner = background_owner()?;
    input_call(
        &owner,
        "ScrollBackground",
        &[
            token.to_string(),
            x.to_string(),
            y.to_string(),
            format!("{:.3}", dx * amount),
            format!("{:.3}", dy * amount),
        ],
    )
}

/// Drag without activating the target; the pointer is restored afterwards.
pub fn drag_background(
    token: u64,
    from: (i32, i32),
    to: (i32, i32),
    button: u8,
) -> anyhow::Result<()> {
    let owner = background_owner()?;
    input_call(
        &owner,
        "DragBackground",
        &[
            token.to_string(),
            from.0.to_string(),
            from.1.to_string(),
            to.0.to_string(),
            to.1.to_string(),
            evdev_button(button).to_string(),
        ],
    )
}

fn type_text_timeout(text: &str) -> Duration {
    let chars = text.chars().count() as u32;
    let worst_case = TYPE_TEXT_WORST_CASE_PER_CHAR
        .checked_mul(chars)
        .unwrap_or(TYPE_TEXT_TIMEOUT_MAX);
    TYPE_TEXT_MARGIN
        .checked_add(worst_case)
        .unwrap_or(TYPE_TEXT_TIMEOUT_MAX)
        .min(TYPE_TEXT_TIMEOUT_MAX)
}

/// Type ASCII text into one exact window's surface without activating it.
pub fn type_text_background(token: u64, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let owner = background_owner()?;
    input_call_with_timeout(
        &owner,
        "TypeTextBackground",
        &[token.to_string(), gvariant_string(text)],
        type_text_timeout(text),
    )
}

/// Press and release one raw evdev key code without activating the target.
pub fn press_key_background(token: u64, keycode: u32) -> anyhow::Result<()> {
    if active_input_token() == Some(token) {
        return press_key(token, keycode);
    }
    let owner = background_owner()?;
    input_call(
        &owner,
        "PressKeyBackground",
        &[token.to_string(), keycode.to_string()],
    )
}

/// Send ordered `(evdev code, pressed)` transitions without activating the target.
pub fn key_sequence_background(token: u64, transitions: &[(u32, bool)]) -> anyhow::Result<()> {
    if active_input_token() == Some(token) {
        return key_sequence(token, transitions);
    }
    if transitions.is_empty() {
        return Ok(());
    }
    let mut payload = String::from("[");
    for (index, (code, pressed)) in transitions.iter().enumerate() {
        if index > 0 {
            payload.push(',');
        }
        payload.push_str(&format!(
            "{{\"code\":{code},\"pressed\":{}}}",
            if *pressed { "true" } else { "false" }
        ));
    }
    payload.push(']');
    let owner = background_owner()?;
    input_call(
        &owner,
        "KeySequenceBackground",
        &[token.to_string(), gvariant_string(&payload)],
    )
}

/// Type ASCII text through the compositor's own keyboard path.
pub fn type_text(token: u64, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let owner = input_owner()?;
    input_call_with_timeout(
        &owner,
        "TypeText",
        &[token.to_string(), gvariant_string(text)],
        type_text_timeout(text),
    )
}

/// Press and release one raw evdev key code.
pub fn press_key(token: u64, keycode: u32) -> anyhow::Result<()> {
    let owner = input_owner()?;
    input_call(
        &owner,
        "PressKey",
        &[token.to_string(), keycode.to_string()],
    )
}

/// Send an ordered list of `(evdev code, pressed)` transitions.
pub fn key_sequence(token: u64, transitions: &[(u32, bool)]) -> anyhow::Result<()> {
    if transitions.is_empty() {
        return Ok(());
    }
    let mut payload = String::from("[");
    for (index, (code, pressed)) in transitions.iter().enumerate() {
        if index > 0 {
            payload.push(',');
        }
        payload.push_str(&format!(
            "{{\"code\":{code},\"pressed\":{}}}",
            if *pressed { "true" } else { "false" }
        ));
    }
    payload.push(']');
    let owner = input_owner()?;
    input_call(
        &owner,
        "KeySequence",
        &[token.to_string(), gvariant_string(&payload)],
    )
}

/// Press at `from`, move along the segment to `to`, then release.
pub fn drag(token: u64, from: (i32, i32), to: (i32, i32), button: u8) -> anyhow::Result<()> {
    let owner = input_owner()?;
    input_call(
        &owner,
        "Drag",
        &[
            token.to_string(),
            from.0.to_string(),
            from.1.to_string(),
            to.0.to_string(),
            to.1.to_string(),
            evdev_button(button).to_string(),
        ],
    )
}

/// Bind the compositor's currently active window for a desktop-scope action.
///
/// Desktop-scope input follows the user's focus instead of naming a target, so
/// this only reads the active token; it never activates anything. Returns
/// `None` when no protocol-v2 helper is loaded, letting the caller fall back to
/// its non-KWin route.
pub fn with_active_window<T>(
    body: impl FnOnce(u64) -> anyhow::Result<T>,
) -> Option<anyhow::Result<T>> {
    let owner = helper_owner()?;
    let token = call_to(&owner, "GetActiveToken", &[]).and_then(|raw| parse_first_i64(&raw))?;
    if token <= 0 {
        return Some(Err(anyhow::anyhow!(
            "KWin reports no active window for desktop-scope input"
        )));
    }
    let token = token as u64;
    let restore = ACTIVE_INPUT_TOKEN.with(|active| active.replace(Some(token)));
    let _guard = InputTokenGuard(restore);
    Some(body(token))
}

/// Look a token up across all live windows, regardless of pid.
pub fn window_for_token(token: u64) -> Option<KwinWindow> {
    snapshot_for_owner(&helper_owner()?)?
        .into_iter()
        .find(|window| window.token == token)
}

fn background_owner() -> anyhow::Result<String> {
    if !supports_background() {
        anyhow::bail!(
            "background_unavailable: the loaded KWin helper does not implement \
             target-bound background input (protocol v{BACKGROUND_PROTOCOL_VERSION})"
        );
    }
    input_owner()
}

fn input_owner() -> anyhow::Result<String> {
    helper_owner().ok_or_else(|| {
        anyhow::anyhow!(
            "foreground_unavailable: no trusted KWin target-input helper (protocol \
             v{PROTOCOL_VERSION}) is loaded"
        )
    })
}

/// Map a cua pointer button (1=left / 2=middle / 3=right) to evdev.
fn evdev_button(button: u8) -> u32 {
    match button {
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        _ => 0x110, // BTN_LEFT
    }
}

/// Quote a string for `gdbus`'s GVariant argument parser.
fn gvariant_string(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '\'' => quoted.push_str("\\'"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c if (c as u32) < 0x20 => quoted.push_str(&format!("\\u{:04x}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('\'');
    quoted
}

/// Call an input method and turn its JSON reply into a Rust result.
fn input_call(owner: &str, method: &str, args: &[String]) -> anyhow::Result<()> {
    input_call_with_timeout(owner, method, args, CALL_TIMEOUT)
}

fn input_call_with_timeout(
    owner: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> anyhow::Result<()> {
    let raw = match call_to_with_timeout(owner, method, args, timeout) {
        Some(raw) => raw,
        None => {
            invalidate_helper_cache();
            anyhow::bail!("KWin helper {method} did not answer");
        }
    };
    let payload = parse_quoted_string(&raw)
        .ok_or_else(|| anyhow::anyhow!("KWin helper {method} returned an unreadable reply"))?;
    let value: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|_| anyhow::anyhow!("KWin helper {method} returned invalid JSON"))?;
    if value
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(());
    }
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown_refusal");
    anyhow::bail!("KWin refused {method}: {reason}")
}

#[derive(Clone)]
struct HelperInfo {
    owner: String,
    version: u32,
    verified: Instant,
}

/// The effect is a long-lived in-process KWin service. Re-discovering it costs
/// five `gdbus` round trips (name owner, PID, UID, version, then the action),
/// which dominated per-action latency. The unique bus name cannot be stolen
/// while its owner lives, so a short verified cache is safe; a dead or
/// replaced effect makes the next input call fail, which clears the cache.
static HELPER_CACHE: Mutex<Option<HelperInfo>> = Mutex::new(None);
const HELPER_CACHE_TTL: Duration = Duration::from_secs(20);

fn helper_info() -> Option<HelperInfo> {
    if let Ok(cache) = HELPER_CACHE.lock() {
        if let Some(info) = cache.as_ref() {
            if info.verified.elapsed() < HELPER_CACHE_TTL {
                return Some(info.clone());
            }
        }
    }

    let owner_raw = dbus_call("GetNameOwner", &[DEST.to_owned()]);
    let owner = parse_quoted_string(&owner_raw?)?;
    if !owner.starts_with(':') {
        return None;
    }

    let pid_raw = gdbus_call_to(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetConnectionUnixProcessID"),
        std::slice::from_ref(&owner),
    );
    let uid_raw = gdbus_call_to(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.GetConnectionUnixUser"),
        std::slice::from_ref(&owner),
    );
    let pid = parse_first_u32(&pid_raw?)?;
    let uid = parse_first_u32(&uid_raw?)?;
    if uid != current_uid() || !is_trusted_kwin(pid, &owner) {
        return None;
    }

    let version_raw = call_to(&owner, "GetVersion", &[]);
    let version = parse_first_u32(&version_raw?)?;
    if version < PROTOCOL_VERSION {
        return None;
    }
    let info = HelperInfo {
        owner,
        version,
        verified: Instant::now(),
    };
    if let Ok(mut cache) = HELPER_CACHE.lock() {
        *cache = Some(info.clone());
    }
    Some(info)
}

fn helper_owner() -> Option<String> {
    helper_info().map(|info| info.owner)
}

fn invalidate_helper_cache() {
    if let Ok(mut cache) = HELPER_CACHE.lock() {
        *cache = None;
    }
}

fn snapshot_for_owner(owner: &str) -> Option<Vec<KwinWindow>> {
    // `gdbus` is already the transport for helper discovery and authorization.
    // Reuse it here instead of opening a fresh zbus session for every read: on
    // a cold session bus that handshake cost seconds per action, while the
    // effect answers the same `GetWindows` call in well under a second.
    for attempt in 0..3 {
        if let Some(raw) = call_to(owner, "GetWindows", &[]) {
            if let Some(text) = parse_gvariant_string(&raw) {
                if let Some(snapshot) = parse_snapshot(&text) {
                    return Some(snapshot);
                }
            }
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    None
}

/// Set a window's frame through the in-process effect. KWin applies the
/// request asynchronously, so poll the attested snapshot until it settles.
pub fn set_window_frame(
    token: u64,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> anyhow::Result<(bool, Option<(i32, i32, u32, u32)>)> {
    let owner = input_owner()?;
    input_call(
        &owner,
        "SetWindowFrame",
        &[
            token.to_string(),
            x.to_string(),
            y.to_string(),
            width.to_string(),
            height.to_string(),
        ],
    )?;
    let mut observed = None;
    for _ in 0..24 {
        std::thread::sleep(Duration::from_millis(25));
        if let Some(window) = list_windows()
            .and_then(|windows| windows.into_iter().find(|window| window.token == token))
        {
            let frame = (window.x, window.y, window.width, window.height);
            observed = Some(frame);
            if frame == (x, y, width, height) {
                return Ok((true, observed));
            }
        }
    }
    Ok((false, observed))
}

/// Press a pointer button without raising the target. The effect keeps the
/// pointer and the held button until [`mouse_up_background`].
pub fn mouse_down_background(token: u64, x: i32, y: i32, button: u8) -> anyhow::Result<()> {
    let owner = background_owner()?;
    input_call(
        &owner,
        "MouseButtonDownBackground",
        &[
            token.to_string(),
            x.to_string(),
            y.to_string(),
            evdev_button(button).to_string(),
        ],
    )
}

/// Move the pointer while a background button is held.
pub fn mouse_move_background(token: u64, x: i32, y: i32) -> anyhow::Result<()> {
    let owner = background_owner()?;
    input_call(
        &owner,
        "MouseMoveBackground",
        &[token.to_string(), x.to_string(), y.to_string()],
    )
}

/// Release a background-held button and restore the pre-drag pointer/active
/// window. The effect releases even when the final point is no longer over the
/// target, so a failed drag cannot leave the user's pointer stuck down.
pub fn mouse_up_background(token: u64, x: i32, y: i32) -> anyhow::Result<()> {
    let owner = background_owner()?;
    input_call(
        &owner,
        "MouseButtonUpBackground",
        &[token.to_string(), x.to_string(), y.to_string()],
    )
}

/// Logical desktop geometry plus per-output records from the effect.
pub fn screen_geometry() -> Option<serde_json::Value> {
    let owner = helper_owner()?;
    let raw = call_to(&owner, "GetScreenGeometry", &[])?;
    let payload = parse_quoted_string(&raw)?;
    serde_json::from_str(&payload).ok()
}

/// Capture one window's own buffer through KWin's ScreenShot2 interface.
/// Unlike a screen crop, this does not include covering windows or the agent
/// cursor. `handle` is the KWin window UUID exposed by `GetWindows`.
pub fn capture_window(token: u64) -> anyhow::Result<Vec<u8>> {
    let window = window_for_token(token).ok_or_else(|| anyhow::anyhow!("unknown_window_token"))?;
    let handle = window.handle;
    std::thread::spawn(move || -> anyhow::Result<Vec<u8>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(capture_window_async(&handle))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("window capture thread panicked"))?
}

/// Capture the whole workspace through KWin's ScreenShot2 interface.
/// This is the fast path for `get_desktop_state`: the generic Wayland
/// cascade can spend many seconds probing wlroots/ext/portal providers
/// before falling back, while KWin answers this in well under a second.
pub fn capture_workspace() -> anyhow::Result<Vec<u8>> {
    std::thread::spawn(|| -> anyhow::Result<Vec<u8>> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(capture_workspace_async())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("workspace capture thread panicked"))?
}

async fn capture_workspace_async() -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};

    use zbus::zvariant::{Fd, Value};

    let mut fds = [0i32; 2];
    // SAFETY: fds points at two valid int slots.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        anyhow::bail!("pipe2 failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created by pipe2 and are owned here.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let mut reader = std::fs::File::from(read_fd);
    let reader_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });

    let connection = session_connection().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        KWIN_DEST,
        "/org/kde/KWin/ScreenShot2",
        "org.kde.KWin.ScreenShot2",
    )
    .await?;
    let mut options: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    options.insert("include-decoration".into(), Value::from(false));
    options.insert("include-cursor".into(), Value::from(false));
    options.insert("include-shadow".into(), Value::from(false));
    options.insert("native-resolution".into(), Value::from(false));
    let result: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = proxy
        .call("CaptureWorkspace", &(options, Fd::from(write_fd)))
        .await?;
    // The Fd argument owns our write end and is closed when `call` returns, so
    // the reader reaches EOF after KWin closes its duplicate.
    let data = reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("screen capture reader panicked"))?;

    let width = value_u32(result.get("width").map(|value| &**value))?;
    let height = value_u32(result.get("height").map(|value| &**value))?;
    let stride = value_u32(result.get("stride").map(|value| &**value))? as usize;
    let format = value_i32(result.get("format").map(|value| &**value))?;
    if width == 0 || height == 0 || stride < width as usize * 4 {
        anyhow::bail!("KWin returned invalid capture geometry");
    }
    if data.len() < stride * height as usize {
        anyhow::bail!(
            "KWin capture returned {} bytes, expected {}",
            data.len(),
            stride * height as usize
        );
    }

    let pixel_bytes = width as usize * 4;
    let mut rgba = Vec::with_capacity(pixel_bytes * height as usize);
    for row in 0..height as usize {
        let line = &data[row * stride..row * stride + pixel_bytes];
        match format {
            // QImage::Format_RGBX8888 has RGBA byte order and opaque alpha.
            16 => {
                for px in line.chunks_exact(4) {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            // QImage::Format_RGBA8888 / RGBA8888_Premultiplied
            17 | 18 => rgba.extend_from_slice(line),
            // QImage::Format_RGB32 / ARGB32 / ARGB32_Premultiplied (BGRA in memory)
            4 | 5 | 6 => {
                for px in line.chunks_exact(4) {
                    rgba.push(px[2]);
                    rgba.push(px[1]);
                    rgba.push(px[0]);
                    rgba.push(if format == 4 { 255 } else { px[3] });
                }
            }
            other => anyhow::bail!("unsupported KWin capture format {other}"),
        }
    }
    if format == 6 || format == 18 {
        for px in rgba.chunks_exact_mut(4) {
            let alpha = px[3] as u16;
            if alpha > 0 && alpha < 255 {
                for channel in &mut px[..3] {
                    *channel = ((u16::from(*channel) * 255) / alpha).min(255) as u8;
                }
            }
        }
    }

    use image::ImageEncoder;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)?;
    Ok(png)
}

async fn capture_window_async(handle: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};

    use zbus::zvariant::{Fd, Value};

    let mut fds = [0i32; 2];
    // SAFETY: fds points at two valid int slots.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        anyhow::bail!("pipe2 failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created by pipe2 and are owned here.
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let mut reader = std::fs::File::from(read_fd);
    let reader_thread = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    });

    let connection = session_connection().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        KWIN_DEST,
        "/org/kde/KWin/ScreenShot2",
        "org.kde.KWin.ScreenShot2",
    )
    .await?;
    let mut options: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    options.insert("include-decoration".into(), Value::from(true));
    options.insert("include-shadow".into(), Value::from(false));
    options.insert("native-resolution".into(), Value::from(false));
    let result: std::collections::HashMap<String, zbus::zvariant::OwnedValue> = proxy
        .call("CaptureWindow", &(handle, options, Fd::from(write_fd)))
        .await?;
    // The Fd argument owns our write end and is closed when `call` returns, so
    // the reader reaches EOF after KWin closes its duplicate.
    let data = reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("screen capture reader panicked"))?;

    let width = value_u32(result.get("width").map(|value| &**value))?;
    let height = value_u32(result.get("height").map(|value| &**value))?;
    let stride = value_u32(result.get("stride").map(|value| &**value))? as usize;
    let format = value_i32(result.get("format").map(|value| &**value))?;
    if width == 0 || height == 0 || stride < width as usize * 4 {
        anyhow::bail!("KWin returned invalid capture geometry");
    }
    if data.len() < stride * height as usize {
        anyhow::bail!(
            "KWin capture returned {} bytes, expected {}",
            data.len(),
            stride * height as usize
        );
    }

    let pixel_bytes = width as usize * 4;
    let mut rgba = Vec::with_capacity(pixel_bytes * height as usize);
    for row in 0..height as usize {
        let line = &data[row * stride..row * stride + pixel_bytes];
        match format {
            // QImage::Format_RGBX8888 has RGBA byte order and opaque alpha.
            16 => {
                for px in line.chunks_exact(4) {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
            // QImage::Format_RGBA8888 / RGBA8888_Premultiplied
            17 | 18 => rgba.extend_from_slice(line),
            // QImage::Format_RGB32 / ARGB32 / ARGB32_Premultiplied (BGRA in memory)
            4 | 5 | 6 => {
                for px in line.chunks_exact(4) {
                    rgba.push(px[2]);
                    rgba.push(px[1]);
                    rgba.push(px[0]);
                    rgba.push(if format == 4 { 255 } else { px[3] });
                }
            }
            other => anyhow::bail!("unsupported KWin capture format {other}"),
        }
    }
    if format == 6 || format == 18 {
        for px in rgba.chunks_exact_mut(4) {
            let alpha = px[3] as u16;
            if alpha > 0 && alpha < 255 {
                for channel in &mut px[..3] {
                    *channel = ((u16::from(*channel) * 255) / alpha).min(255) as u8;
                }
            }
        }
    }

    use image::ImageEncoder;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)?;
    Ok(png)
}

fn value_u32(value: Option<&zbus::zvariant::Value>) -> anyhow::Result<u32> {
    let value = value.ok_or_else(|| anyhow::anyhow!("missing capture metadata"))?;
    if let Ok(v) = value.downcast_ref::<u32>() {
        return Ok(v);
    }
    if let Ok(v) = value.downcast_ref::<i32>() {
        return u32::try_from(v).map_err(|_| anyhow::anyhow!("negative capture metadata"));
    }
    anyhow::bail!("unexpected capture metadata type")
}

fn value_i32(value: Option<&zbus::zvariant::Value>) -> anyhow::Result<i32> {
    let value = value.ok_or_else(|| anyhow::anyhow!("missing capture metadata"))?;
    if let Ok(v) = value.downcast_ref::<i32>() {
        return Ok(v);
    }
    if let Ok(v) = value.downcast_ref::<u32>() {
        return i32::try_from(v).map_err(|_| anyhow::anyhow!("capture metadata overflow"));
    }
    anyhow::bail!("unexpected capture metadata type")
}

pub fn parse_snapshot(raw: &str) -> Option<Vec<KwinWindow>> {
    // `raw` is the actual QString payload decoded by zbus, not gdbus's
    // human-readable GVariant rendering. Parsing the display representation is
    // unsafe because GLib may change its quoting/escaping for valid titles.
    let values: Vec<serde_json::Value> = serde_json::from_str(raw).ok()?;

    let mut tokens = HashSet::new();
    let mut active_count = 0usize;
    values
        .into_iter()
        .map(|value| {
            let token = value
                .get("token")
                .and_then(serde_json::Value::as_u64)
                .filter(|token| *token > 0)?;
            if !tokens.insert(token) {
                return None;
            }

            let active = value.get("active")?.as_bool()?;
            if active {
                active_count += 1;
                if active_count > 1 {
                    return None;
                }
            }

            Some(KwinWindow {
                token,
                handle: value
                    .get("handle")
                    .and_then(serde_json::Value::as_str)?
                    .to_owned(),
                pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
                title: value
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                app_name: value
                    .get("app_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                x: json_i32(&value, "x")?,
                y: json_i32(&value, "y")?,
                width: json_u32(&value, "w")?,
                height: json_u32(&value, "h")?,
                active,
                minimized: value.get("minimized")?.as_bool()?,
                stacking: usize::try_from(value.get("stacking")?.as_u64()?).ok()?,
                transient_for: value
                    .get("transient_for")
                    .and_then(serde_json::Value::as_u64)
                    .filter(|id| *id != 0),
                modal: value.get("modal").and_then(serde_json::Value::as_bool),
                window_type: value
                    .get("window_type")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn json_i32(value: &serde_json::Value, key: &str) -> Option<i32> {
    let number = value.get(key)?.as_f64()?;
    (number.is_finite() && number >= i32::MIN as f64 && number <= i32::MAX as f64)
        .then(|| number.round() as i32)
}

fn json_u32(value: &serde_json::Value, key: &str) -> Option<u32> {
    let number = value.get(key)?.as_f64()?;
    (number.is_finite() && number >= 0.0 && number <= u32::MAX as f64)
        .then(|| number.round() as u32)
}

fn dbus_call(method: &str, args: &[String]) -> Option<String> {
    gdbus_call_to(
        DBUS_DEST,
        DBUS_PATH,
        &format!("{DBUS_IFACE}.{method}"),
        args,
    )
}

fn call_to(owner: &str, method: &str, args: &[String]) -> Option<String> {
    call_to_with_timeout(owner, method, args, CALL_TIMEOUT)
}

fn call_to_with_timeout(
    owner: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Option<String> {
    gdbus_call_to_with_timeout(owner, PATH, &format!("{IFACE}.{method}"), args, timeout)
}

fn gdbus_call_to(
    destination: &str,
    object_path: &str,
    method: &str,
    args: &[String],
) -> Option<String> {
    gdbus_call_to_with_timeout(destination, object_path, method, args, CALL_TIMEOUT)
}

// One I/O runtime and bus connection serve all KWin metadata and input calls.
// Keep the reader alive between calls; reconnect only after a transport error.
static BUS_RUNTIME: std::sync::OnceLock<Option<tokio::runtime::Runtime>> =
    std::sync::OnceLock::new();
static BUS_CONNECTION: tokio::sync::Mutex<Option<zbus::Connection>> =
    tokio::sync::Mutex::const_new(None);

fn bus_runtime() -> Option<&'static tokio::runtime::Runtime> {
    BUS_RUNTIME
        .get_or_init(|| {
            // Initialization can originate on the daemon's Tokio runtime.
            // Construct the independent runtime on an ordinary thread.
            std::thread::spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .thread_name("kwin-dbus")
                    .build()
                    .ok()
            })
            .join()
            .ok()
            .flatten()
        })
        .as_ref()
}

async fn session_connection() -> anyhow::Result<zbus::Connection> {
    let mut cached = BUS_CONNECTION.lock().await;
    if let Some(connection) = cached.as_ref() {
        return Ok(connection.clone());
    }
    let connection = bus_runtime()
        .ok_or_else(|| anyhow::anyhow!("KWin bus runtime unavailable"))?
        .spawn(zbus::Connection::session())
        .await??;
    *cached = Some(connection.clone());
    Ok(connection)
}

/// Build an exact typed D-Bus body. Unknown signatures fail before dispatch.
fn native_call_body(
    method: &str,
    args: &[String],
) -> Option<Option<zbus::zvariant::Structure<'static>>> {
    use zbus::zvariant::{StructureBuilder, Value};
    let signature = match method {
        "GetNameOwner" | "GetConnectionUnixProcessID" | "GetConnectionUnixUser" => "s",
        "Activate"
        | "BeginAccessibilityProbe"
        | "AdvanceAccessibilityProbe"
        | "EndAccessibilityProbe" => "x",
        "Move" | "MouseMoveBackground" | "MouseButtonUpBackground" => "xii",
        "MovePointer" => "dd",
        "Click" | "ClickBackground" => "xiiui",
        "Scroll" | "ScrollBackground" => "xiidd",
        "Drag" | "DragBackground" => "xiiiiu",
        "TypeText" | "TypeTextBackground" | "KeySequence" | "KeySequenceBackground" => "xS",
        "PressKey" | "PressKeyBackground" => "xu",
        "SetWindowFrame" => "xiiii",
        "MouseButtonDownBackground" => "xiiu",
        "GetVersion" | "GetBuild" | "GetWindows" | "GetActiveToken" | "GetPointer"
        | "GetScreenGeometry" | "GetFocusState" => "",
        _ => return None,
    };
    if signature.len() != args.len() {
        return None;
    }
    if args.is_empty() {
        return Some(None);
    }
    let mut builder = StructureBuilder::new();
    for (kind, argument) in signature.chars().zip(args) {
        let value = match kind {
            'x' => Value::from(argument.parse::<i64>().ok()?),
            'i' => Value::from(argument.parse::<i32>().ok()?),
            'u' => Value::from(argument.parse::<u32>().ok()?),
            'd' => Value::from(argument.parse::<f64>().ok()?),
            's' => Value::from(argument.clone()),
            'S' => Value::from(parse_gvariant_string(argument)?),
            _ => return None,
        };
        builder = builder.append_field(value);
    }
    Some(Some(builder.build().ok()?))
}

fn gdbus_call_to_with_timeout(
    destination: &str,
    object_path: &str,
    method: &str,
    args: &[String],
    timeout: Duration,
) -> Option<String> {
    let (interface, member) = method.rsplit_once('.')?;
    let body = native_call_body(member, args)?;
    let destination = destination.to_owned();
    let object_path = object_path.to_owned();
    let interface = interface.to_owned();
    let member = member.to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let task = bus_runtime()?.spawn(async move {
        let result: anyhow::Result<String> = async {
            let connection = session_connection().await?;
            let reply = match body {
                Some(body) => {
                    connection
                        .call_method(
                            Some(destination.as_str()),
                            object_path.as_str(),
                            Some(interface.as_str()),
                            member.as_str(),
                            &body,
                        )
                        .await
                }
                None => {
                    connection
                        .call_method(
                            Some(destination.as_str()),
                            object_path.as_str(),
                            Some(interface.as_str()),
                            member.as_str(),
                            &(),
                        )
                        .await
                }
            };
            let reply = match reply {
                Ok(reply) => reply,
                Err(error) => {
                    // Do not retry a dispatched input call: the effect may
                    // have applied it already. Reconnect on the next request.
                    BUS_CONNECTION.lock().await.take();
                    return Err(error.into());
                }
            };
            let body = reply.body();
            if let Ok((value,)) = body.deserialize::<(String,)>() {
                return Ok(format!("({},)", gvariant_string(&value)));
            }
            if let Ok((value,)) = body.deserialize::<(u32,)>() {
                return Ok(format!("(uint32 {value},)"));
            }
            if let Ok((value,)) = body.deserialize::<(i64,)>() {
                return Ok(format!("(int64 {value},)"));
            }
            anyhow::bail!("unexpected KWin D-Bus reply type")
        }
        .await;
        let _ = sender.send(result.ok());
    });
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            task.abort();
            None
        }
    }
}

/// Decode the single-quoted string that `gdbus call` prints, honoring
/// GVariant escapes so a window title containing an apostrophe or backslash
/// does not truncate the JSON payload.
fn parse_gvariant_string(raw: &str) -> Option<String> {
    let start = raw.find('\'')? + 1;
    let mut out = String::new();
    let mut chars = raw[start..].chars();
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'u' => {
                    let digits: String = chars.by_ref().take(4).collect();
                    if digits.len() != 4 {
                        return None;
                    }
                    let code = u32::from_str_radix(&digits, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                other => out.push(other),
            },
            '\'' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

fn parse_quoted_string(raw: &str) -> Option<String> {
    let start = raw.find('\'')? + 1;
    let end = raw[start..].find('\'')? + start;
    (end > start).then(|| raw[start..end].to_owned())
}

fn parse_first_u32(raw: &str) -> Option<u32> {
    let payload = raw.split_once("uint32").map_or(raw, |(_, payload)| payload);
    payload
        .split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

fn parse_first_i64(raw: &str) -> Option<i64> {
    let payload = raw.split_once("int64").map_or(raw, |(_, payload)| payload);
    let start = payload.find(|c: char| c == '-' || c.is_ascii_digit())?;
    payload[start..]
        .split(|character: char| !(character.is_ascii_digit() || character == '-'))
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

fn current_uid() -> u32 {
    std::fs::metadata(format!("/proc/{}", std::process::id()))
        .map(|meta| meta.uid())
        .unwrap_or(u32::MAX)
}

fn is_trusted_kwin(pid: u32, helper_owner: &str) -> bool {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok();
    if comm.as_deref().map(str::trim) != Some("kwin_wayland") {
        return false;
    }

    let Some(owner_raw) = dbus_call("GetNameOwner", &[KWIN_DEST.to_owned()]) else {
        return false;
    };
    let Some(kwin_owner) = parse_quoted_string(&owner_raw) else {
        return false;
    };
    if kwin_owner != helper_owner {
        return false;
    }

    let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    match executable {
        Some(path) => {
            let metadata = std::fs::metadata(&path).ok();
            path.file_name().and_then(|name| name.to_str()) == Some("kwin_wayland")
                && metadata
                    .is_some_and(|meta| meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0)
        }
        None => true,
    }
}

#[cfg(test)]
mod speed_transport_tests {
    use super::*;

    #[test]
    fn compositor_method_bodies_match_introspected_signatures() {
        for (method, args, expected) in [
            (
                "ClickBackground",
                vec!["7", "10", "20", "272", "2"],
                "(xiiui)",
            ),
            (
                "ScrollBackground",
                vec!["7", "10", "20", "0.0", "-3.0"],
                "(xiidd)",
            ),
            (
                "SetWindowFrame",
                vec!["7", "-10", "20", "800", "650"],
                "(xiiii)",
            ),
            ("TypeTextBackground", vec!["7", "'hello'"], "(xs)"),
            ("GetNameOwner", vec!["org.cua.KWinTarget"], "(s)"),
        ] {
            let args: Vec<String> = args.into_iter().map(str::to_owned).collect();
            let body = native_call_body(method, &args).unwrap().unwrap();
            assert_eq!(body.signature().to_string(), expected, "{method}");
        }
    }

    #[test]
    fn malformed_bodies_are_refused_before_dispatch() {
        assert!(native_call_body("Unknown", &[]).is_none());
        assert!(native_call_body("Activate", &[]).is_none());
        assert!(native_call_body("PressKey", &["7".into(), "-1".into()]).is_none());
        assert!(native_call_body("TypeText", &["7".into(), "'\\uGGGG'".into()]).is_none());
        assert!(parse_gvariant_string("'\\u12'").is_none());
        assert!(parse_gvariant_string("'\\ud800'").is_none());
        assert!(native_call_body("GetWindows", &[]).unwrap().is_none());
    }

    #[test]
    fn text_arguments_preserve_quotes_unicode_and_control_escapes() {
        for text in [
            "can't \\ quote",
            "Café 世界 ✓",
            "first\nsecond\tline",
            "back\u{0008}space\u{001b}",
            r"\u0041 stays literal",
        ] {
            assert_eq!(
                parse_gvariant_string(&gvariant_string(text)).as_deref(),
                Some(text)
            );
        }
        let controls: String = (0u8..32).map(char::from).collect();
        assert_eq!(
            parse_gvariant_string(&gvariant_string(&controls)),
            Some(controls)
        );
    }
}

#[cfg(test)]
mod popup_metadata_tests {
    use super::*;
    #[test]
    fn snapshot_relations_are_optional_and_preserve_native_ids() {
        let mut value = serde_json::json!({"token":2,"handle":"popup-uuid","pid":7,"x":1,"y":2,"w":204,"h":110,"active":false,"minimized":false,"stacking":4});
        let old = parse_snapshot(&serde_json::json!([value.clone()]).to_string())
            .unwrap()
            .remove(0);
        assert_eq!(old.transient_for, None);
        assert_eq!(old.modal, None);
        assert_eq!(old.window_type, None);
        value["transient_for"] = serde_json::json!(1);
        value["modal"] = serde_json::json!(false);
        value["window_type"] = serde_json::json!("popup");
        let new = parse_snapshot(&serde_json::json!([value]).to_string())
            .unwrap()
            .remove(0);
        assert_eq!(new.transient_for, Some(1));
        assert_eq!(new.modal, Some(false));
        assert_eq!(new.window_type.as_deref(), Some("popup"));
    }
}
