# Cua KWin target helper

This is an **optional KDE Plasma/KWin Wayland integration**. It is not part of
the portable `cua-driver` binary and must be built against the user's installed
KWin/Qt/KF6 development files. Do not use a helper built for a different KWin
or Qt ABI.

This fork adds protocol-v3 input methods alongside trusted window metadata.
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

Bulk text and Unicode use clipboard paste. Clipboard transactions preserve
native payload formats, support empty selections and retain a different text
value copied during dispatch. Konsole uses Ctrl+Shift+V. Short text and control
characters retain physical key semantics. Background scrolling acknowledges
after pointer/focus/stack restoration, preserving sequential batch ordering.

Window and workspace captures use KWin ScreenShot2, including direct RGBX8888
decoding. Window captures exclude separate popup and tooltip windows; use a
desktop capture for those. Drag requests run sequentially because the
compositor has one synthetic pointer.

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
