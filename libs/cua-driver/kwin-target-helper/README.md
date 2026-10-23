# Cua KWin target helper

This is an **optional KDE Plasma/KWin Wayland integration**. It is not part of
the portable `cua-driver` binary and must be built against the user's installed
KWin/Qt/KF6 development files. Do not use a helper built for a different KWin
or Qt ABI.

This fork adds protocol-v3 input, protocol-v4 accessibility identity methods,
and protocol-v5 dialog and popup metadata.
The helper resolves opaque window tokens inside KWin, validates callers and
targets, and delivers input through the compositor. The driver verifies that
the D-Bus service belongs to the current user's KWin process and binds the
requested PID to its window token.

Background pointer actions temporarily raise occluded targets and restore the
pointer, previous active window and stacking order. Keyboard and wheel actions
briefly activate their target. Applications observe those focus changes.

## Requirements

- KDE Plasma/KWin 6 with a matching KWin development package
- Qt 6 development files
- ECM/KF6CoreAddons development files
- CMake 3.21 or newer
- a running Wayland KWin session for loading/testing

On Arch-derived systems, install the distribution's matching packages (package
names vary by distribution), then build from this directory:

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build --parallel
```

The module is installed in KWin's effect plugin namespace:

```text
kwin/effects/plugins/cua_kwin_target_helper.so
```

For a user-local installation, use a prefix supported by the local Qt/KWin
plugin search path and verify discovery before loading:

```bash
cmake --install build --prefix "$HOME/.local"
```

If the local KWin installation does not search that prefix, use the
distribution's native packaging mechanism or an explicitly approved
system-local installation. Never copy the module into an arbitrary plugin
directory and never overwrite an existing file.

Load/reload through KWin's supported Effects interface where available:

```bash
qdbus6 org.kde.KWin /Effects loadEffect cua_kwin_target_helper
```

The helper exposes the Cua-owned service `org.cua.KWinTarget` only while the
effect is loaded by the current `kwin_wayland` process. Verify the service
owner, protocol version, and target snapshots before trusting identity data.
The D-Bus surface includes version/build and window metadata, activation,
geometry, pointer and keyboard methods, and background variants. The driver
uses a persistent native D-Bus connection rather than a subprocess per call.

## Input safety boundary

Target validation and input delivery run inside the compositor. Shortcut
transitions yield through timers so applications can process modifier changes.
Every keydown revalidates the target. Keyups still release when a shortcut
closes the window, and aborted requests release their owned keys. Overlapping
writes return `input_busy` while metadata remains readable. The driver does
not replay dispatched input automatically after a transport error.

Accessibility rows require an independently matched native window. Geometry
matching accounts for Wayland's window-local origins and requires an unambiguous
match in both directions. Windows with identical titles and sizes use a bounded
focus calibration when no lifetime identity has been established. The helper
briefly clears focus and focuses the exact target twice, without raising it or
sending keys, clicks or pointer motion. The driver requires the same accessible
frame to acknowledge both leave/enter sequences. Compositor focus epochs detect
intervening changes, including a move away and back. The helper restores the
previous focus and stacking order before the read completes. Applications can
observe these transient focus changes.

The mapping joins KWin's window UUID to the accessibility bus/object reference.
Later reads reuse that mapping without changing focus, including after title,
size or focus changes. Closed windows and replaced accessibility objects lose
their bindings. A scoped tree walk visits only the requested window, so an
unresponsive sibling does not consume its budget. Input cannot overlap a
calibration; the helper releases an
abandoned lease after 1.5 seconds. A user's intervening focus change supersedes
restoration. `GetFocusState`, `BeginAccessibilityProbe`,
`AdvanceAccessibilityProbe` and `EndAccessibilityProbe` require helper protocol
v4. Earlier helpers retain ordinary title/geometry matching.

When accessibility is absent, a calibration is interrupted or the caller's
deadline expires, the driver withholds an unproven tree. The response marks
`accessibility_window_scoped: false`, `elements_complete: false` and
`degraded_reason: accessibility_window_identity_unproven: ...`, with no
unrelated rows or actionable element snapshot. Previous element snapshots for
that window are invalidated. A screenshot-only read cannot revive their tokens.
Use screenshot coordinates or retry after the interruption in that case.

Bulk text and Unicode use clipboard paste. Clipboard transactions preserve
native payload formats, support empty selections and retain a different text
value copied during dispatch. Konsole uses Ctrl+Shift+V. Short text and control
characters retain physical key semantics. Background scrolling acknowledges
after pointer/focus/stack restoration, preserving sequential batch ordering.

Window and workspace captures use KWin ScreenShot2, including direct RGBX8888
decoding. A parent window's capture contains its own buffer. Protocol v5 also
lists separate user-owned popup and tooltip windows with their own tokens,
UUIDs, geometry, native `transient_for`, `modal`, and `window_type`. Shell
panels, notifications, and driver overlays remain excluded. The shared Linux
responses preserve these fields and list native descendants in `dialogs` and
`popups`, including children owned by another process. Follow-up calls name
that child's PID. Same-PID siblings and overlapping unrelated windows do not
imply ownership.

Call `get_window_state` with the popup's own PID and window ID for its complete
image, including pixels outside the parent. The response publishes a separate
capture ID for coordinate actions. Popup reads do not activate the popup or
borrow focus, and single clicks validate its actual pointer target without raising it.
Multiple-click popup requests are refused before input, since the first click
can close the menu. Toplevel-only input and geometry methods do not accept
popup tokens.
When Qt publishes a menu below a menu bar, the adapter searches the proved
parent's accessibility subtree and treats the showing menu as its own root.
Popup dimensions must match tightly and uniquely in both directions. Ambiguous
or absent accessible roots still withhold unrelated rows; the popup's image
and capture-bound coordinate actions remain available. Applications that do
not export a context menu through AT-SPI cannot supply element tokens for it.
Drag requests run sequentially because the compositor has one synthetic pointer.

The shared metadata plumbing also preserves Mutter's native parent, modal and
window-type fields in bundled GNOME helper API 11, and the wlroots foreign
toplevel protocol's v3 parent event. This does not make every compositor expose
popup handles. GNOME only reports windows represented by Mutter window actors;
wlroots foreign-toplevel and ext-foreign-toplevel enumerate toplevels rather
than arbitrary popup surfaces. Hyprland's current clients IPC path has no
native parent metadata in this adapter. Missing provider fields remain absent,
and no parent is guessed from titles, PID or overlap. Sway-enriched parent IDs
require a unique title/app identity join. Those other sessions need their own
native runtime verification.

## Verification and limits

Verified on a two-core KDE Plasma 6.8 Wayland VM with one output. Release builds
and focused transport/native clipboard tests passed. Twelve browser/terminal
checks passed under two temporary CPU workers and 576 MiB of memory load.
Checks included actual destination pages, exact saved text, Unicode terminal
execution, foreground/background shortcuts, Backspace, overlap refusal, key
release after window closure, scrolling and capture color/alpha. The broader
Cargo test-binary build was cancelled after memory contention; the complete
Cargo suite and cross-platform desktop matrix were not completed.

Representative CLI measurements inside the VM, excluding SSH and model time:

| Action | Before optimization | After optimization |
| --- | ---: | ---: |
| Click | 1.424 s | 0.245 s |
| Scroll | 3.344 s | 0.828 s |
| Shortcut | 3.368 s | 0.782 s |
| Desktop capture | 1.422 s | 0.436 s |
| Type 200 ASCII characters | 4.840 s | 1.657 s |

Repeated calls use medians; the text case is one sample. A later three-run
typing check took 1.066–1.782 seconds, so sub-second typing is not established.
The optional agent cursor overlay was disabled for these measurements.
More aggressive shortcut timings missed a Save and were discarded.
Multi-monitor behavior and other compositor/Qt versions require verification.

## ABI and rollback

The module is compositor/Qt ABI-specific. Rebuild after Plasma/KWin or Qt
upgrades. To roll back a user-local test installation, unload the effect and
remove only the exact module installed by that test; do not remove KWin or
change the desktop session. The portable Cua Driver continues to build without
this optional helper and without KWin development packages.
