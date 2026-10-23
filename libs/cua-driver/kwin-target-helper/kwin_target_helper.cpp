// SPDX-License-Identifier: MIT
// Cua-owned KWin effect.
//
// Protocol v2: the v1 read-only window identity surface (GetVersion,
// GetWindows) plus target-bound input methods.
//
// Every input method:
//   1. verifies the D-Bus caller runs as the same Unix user,
//   2. resolves the opaque token to a live KWin window,
//   3. forces activation of that exact window and confirms it is active,
//   4. for pointer operations, confirms the pointer is over that window,
// and only then injects through KWin's own input redirection. The activation
// and the injection happen in the same compositor process with no cross-process
// round trip, so there is no focus-change window between check and delivery.

#include <kwin/core/inputdevice.h>
#include <kwin/core/output.h>
#include <kwin/effect/effect.h>
#include <kwin/effect/effecthandler.h>
#include <kwin/input.h>
#include <kwin/keyboard_input.h>
#include <kwin/pointer_input.h>
#include <kwin/window.h>
#include <kwin/workspace.h>
#include <kwin/wayland_server.h>
#include <kwin/wayland/seat.h>
#include <kwin/wayland/surface.h>

#include <QCoreApplication>
#include <QDBusConnection>
#include <QDBusConnectionInterface>
#include <QDBusContext>
#include <QDBusMessage>
#include <QElapsedTimer>
#include <QHash>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QPointer>
#include <QSet>
#include <QThread>
#include <QTimer>
#include <QtMath>
#include <QUuid>
#include <unistd.h>

#include <chrono>

namespace
{
constexpr auto kService = "org.cua.KWinTarget";
constexpr auto kPath = "/org/cua/KWinTarget";
constexpr quint32 kProtocolVersion = 5;
constexpr auto kBuild = "2026-10-23.3-popup-target-guards";

using Microseconds = std::chrono::microseconds;

Microseconds nowUs()
{
    return std::chrono::duration_cast<Microseconds>(
        std::chrono::steady_clock::now().time_since_epoch());
}

QString okJson()
{
    return QStringLiteral("{\"ok\":true}");
}

QString errJson(const QString &reason)
{
    QJsonObject object;
    object[QStringLiteral("ok")] = false;
    object[QStringLiteral("reason")] = reason;
    return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
}

// evdev key codes for a US layout: the only layout Cua promises for text.
struct KeyStroke
{
    uint32_t code;
    bool shift;
};

bool asciiStroke(QChar character, KeyStroke *stroke)
{
    const ushort c = character.unicode();
    // evdev key codes are positional, not alphabetical.
    static constexpr uint32_t kLetters[26] = {
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
        49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    };
    if (c >= 'a' && c <= 'z') {
        *stroke = {kLetters[c - 'a'], false};
        return true;
    }
    if (c >= 'A' && c <= 'Z') {
        *stroke = {kLetters[c - 'A'], true};
        return true;
    }
    if (c >= '1' && c <= '9') {
        *stroke = {static_cast<uint32_t>(2 + (c - '1')), false};
        return true;
    }
    switch (c) {
    case '0': *stroke = {11, false}; return true;
    case '-': *stroke = {12, false}; return true;
    case '=': *stroke = {13, false}; return true;
    case '[': *stroke = {26, false}; return true;
    case ']': *stroke = {27, false}; return true;
    case ';': *stroke = {39, false}; return true;
    case '\'': *stroke = {40, false}; return true;
    case '`': *stroke = {41, false}; return true;
    case '\\': *stroke = {43, false}; return true;
    case ',': *stroke = {51, false}; return true;
    case '.': *stroke = {52, false}; return true;
    case '/': *stroke = {53, false}; return true;
    case ' ': *stroke = {57, false}; return true;
    case '!': *stroke = {2, true}; return true;
    case '@': *stroke = {3, true}; return true;
    case '#': *stroke = {4, true}; return true;
    case '$': *stroke = {5, true}; return true;
    case '%': *stroke = {6, true}; return true;
    case '^': *stroke = {7, true}; return true;
    case '&': *stroke = {8, true}; return true;
    case '*': *stroke = {9, true}; return true;
    case '(': *stroke = {10, true}; return true;
    case ')': *stroke = {11, true}; return true;
    case '_': *stroke = {12, true}; return true;
    case '+': *stroke = {13, true}; return true;
    case '{': *stroke = {26, true}; return true;
    case '}': *stroke = {27, true}; return true;
    case ':': *stroke = {39, true}; return true;
    case '"': *stroke = {40, true}; return true;
    case '~': *stroke = {41, true}; return true;
    case '|': *stroke = {43, true}; return true;
    case '<': *stroke = {51, true}; return true;
    case '>': *stroke = {52, true}; return true;
    case '?': *stroke = {53, true}; return true;
    case '\n': *stroke = {28, false}; return true;
    case '\t': *stroke = {15, false}; return true;
    case '\b': *stroke = {14, false}; return true;
    case 0x1b: *stroke = {1, false}; return true;
    default: return false;
    }
}

constexpr uint32_t kLeftShift = 42;
// Keep the original 10 ms transition pacing. Bulk text uses clipboard paste.
void keyGap()
{
    QThread::msleep(10);
}

void flushPointerFrame()
{
    KWin::input()->pointer()->processFrame();
}

// A motion to the point the real pointer already occupies may not re-run
// KWin's pointer-focus pick. Move one pixel first so wheel and hover go to the
// window that is topmost after a transient raise, not to the window that was
// under the stale pointer focus.
void movePointerToPoint(const QPointF &point)
{
    auto *pointer = KWin::input()->pointer();
    QPointF approach = point;
    if (approach.x() > 0.0) {
        approach.setX(approach.x() - 1.0);
    } else {
        approach.setX(approach.x() + 1.0);
    }
    pointer->processMotionAbsolute(approach, nowUs(), nullptr);
    flushPointerFrame();
    pointer->processMotionAbsolute(point, nowUs(), nullptr);
    flushPointerFrame();
}

// KWin's window list also carries layer-shell surfaces (Plasma panels, the
// desktop, notifications) and Cua's own overlay. The stock Wayland adapters
// enumerate xdg_toplevel clients only, so hide everything else from window
// discovery to keep list_windows/get_desktop_state identical.
bool isUserToplevel(KWin::Window *window)
{
    if (!window || !window->isClient() || window->isDeleted()) {
        return false;
    }
    if (window->isDesktop() || window->isDock() || window->isMenu()
        || window->isDropdownMenu() || window->isPopupMenu()
        || window->isTooltip() || window->isNotification()
        || window->isCriticalNotification() || window->isAppletPopup()
        || window->isOnScreenDisplay() || window->isComboBox()
        || window->isDNDIcon() || window->isLockScreen()
        || window->isInputMethod() || window->isOutline()
        || window->isPopupWindow()) {
        return false;
    }
    return window->resourceClass() != QLatin1String("cua-driver");
}

bool isUserPopup(KWin::Window *window)
{
    if (!window || window->isDeleted() || !window->surface()
        || !(window->isPopupWindow() || window->isDropdownMenu() || window->isPopupMenu()
             || window->isTooltip() || window->isComboBox())) {
        return false;
    }
    // Only expose popups whose compositor-owned parent chain reaches a user
    // window. Plasma panels, notifications and Cua overlays stay excluded.
    QSet<KWin::Window *> seen;
    for (auto *parent = window->transientFor(); parent && !seen.contains(parent);
         parent = parent->transientFor()) {
        if (parent->isDeleted()) return false;
        if (isUserToplevel(parent)) return true;
        seen.insert(parent);
    }
    return false;
}

bool isUserSurface(KWin::Window *window)
{
    return isUserToplevel(window) || isUserPopup(window);
}

// KWin's normal pointer path activates a window on button press. Background
// delivery must not disturb the user's active window, so capture it up front
// and restore it once the injected gesture is finished. Background keyboard
// actions also use this guard after transiently activating the target.
class ActiveWindowGuard
{
public:
    ActiveWindowGuard()
        : m_window(KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr)
    {
    }

    explicit ActiveWindowGuard(KWin::Window *window)
        : m_window(window)
    {
    }

    ~ActiveWindowGuard()
    {
        if (m_window && KWin::workspace() && KWin::workspace()->activeWindow() != m_window) {
            KWin::workspace()->activateWindow(m_window, true);
        }
    }

    void disarm()
    {
        m_window = nullptr;
    }

private:
    KWin::Window *m_window = nullptr;
};

// Background pointer actions must be able to hit an occluded target. KWin
// routes synthetic pointer events by stacking order, so raise the target for
// the duration of the gesture and then put every user toplevel back in its
// original order. This is intentionally transient: no paint happens between
// raise, injection, and restore in the compositor event loop. The previous
// active window is restored separately by ActiveWindowGuard; declare this
// guard first so it restores stacking after focus has been restored.
class StackingGuard
{
public:
    explicit StackingGuard(KWin::Window *target, bool raiseTarget = true)
    {
        auto *workspace = KWin::workspace();
        if (!workspace) {
            return;
        }
        m_order = workspace->stackingOrder();
        if (raiseTarget) {
            workspace->raiseWindow(target, true);
            m_raised = true;
        }
    }

    ~StackingGuard()
    {
        restore();
    }

    void restore()
    {
        if (m_restored || m_order.isEmpty()) {
            return;
        }
        m_restored = true;
        auto *workspace = KWin::workspace();
        if (!workspace) {
            return;
        }
        for (KWin::Window *window : std::as_const(m_order)) {
            if (!window || window->isDeleted() || window->isMinimized()
                || !isUserToplevel(window)) {
                continue;
            }
            workspace->raiseWindow(window, true);
        }
        // Restoring stacking does not update KWin's pointer focus by itself.
        // The real pointer never moved, so re-publish its saved position to
        // put hover/click focus back on the window the user can see.
        const QPointF current = KWin::input()->globalPointer();
        KWin::input()->pointer()->processMotionAbsolute(current, nowUs(), nullptr);
        flushPointerFrame();
    }

    bool raised() const
    {
        return m_raised;
    }

    const QList<KWin::Window *> &order() const
    {
        return m_order;
    }

    void disarm()
    {
        m_restored = true;
    }

private:
    QList<KWin::Window *> m_order;
    bool m_raised = false;
    bool m_restored = false;
};

class TargetApi : public QObject, protected QDBusContext
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "org.cua.KWinTarget")

public:
    TargetApi()
    {
        connect(KWin::workspace(), &KWin::Workspace::windowActivated, this,
                [this](KWin::Window *) { ++m_focusEpoch; });
        connect(KWin::waylandServer()->seat(), &KWin::SeatInterface::focusedKeyboardSurfaceAboutToChange,
                this, [this](KWin::SurfaceInterface *) { ++m_focusEpoch; });
    }

    ~TargetApi() override
    {
        finishAccessibilityProbe();
        if (m_keyboardOwner) {
            for (uint32_t code : std::as_const(m_ownedKeys)) {
                m_keyboardOwner->processKey(code, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
            }
        }
    }

public slots:
    Q_SCRIPTABLE quint32 GetVersion()
    {
        return kProtocolVersion;
    }

    Q_SCRIPTABLE QString GetBuild()
    {
        return QString::fromLatin1(kBuild);
    }

    // Epoch brackets detect intervening focus changes, including an A/B/A
    // round trip. Pointer focus, window order and geometry are not identity.
    Q_SCRIPTABLE QString GetFocusState()
    {
        QJsonObject object;
        auto *window = KWin::workspace()->activeWindow();
        object["epoch"] = static_cast<qint64>(m_focusEpoch);
        object["active_token"] = window ? static_cast<qint64>(tokenFor(window->internalId())) : 0;
        object["keyboard_token"] = keyboardFocusToken();
        object["calibrating"] = m_probeLease != 0;
        return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString BeginAccessibilityProbe(qint64 token)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy || m_holdActive) {
            return errJson(QStringLiteral("input_busy"));
        }
        auto *window = windowForToken(token);
        if (!window || window->isMinimized() || isUserPopup(window)) {
            return errJson(QStringLiteral("target_unavailable"));
        }
        m_probePrevious = KWin::workspace()->activeWindow();
        m_probePreviousKeyboard = keyboardFocusToken();
        m_probeTarget = window;
        m_probeCaller = calledFromDBus() ? message().service() : QString();
        m_probeOrder.clear();
        for (auto *item : KWin::workspace()->stackingOrder()) {
            m_probeOrder.push_back(item);
        }
        const quint64 lease = ++m_nextProbeLease;
        m_probeLease = lease;
        m_inputBusy = true;
        // First require the client to acknowledge focus leaving. Otherwise a
        // delayed active-state event from another window could be misbound.
        m_probePhase = 0;
        KWin::workspace()->activateWindow(nullptr);
        m_probeEpoch = m_focusEpoch;
        if (KWin::workspace()->activeWindow() || keyboardFocusToken() != 0) {
            finishAccessibilityProbe();
            return errJson(QStringLiteral("focus_not_confirmed"));
        }
        QTimer::singleShot(1500, this, [this, lease]() {
            if (m_probeLease == lease) {
                finishAccessibilityProbe();
            }
        });
        QJsonObject object;
        object["ok"] = true;
        object["lease"] = static_cast<qint64>(lease);
        object["epoch"] = static_cast<qint64>(m_probeEpoch);
        return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString AdvanceAccessibilityProbe(qint64 lease)
    {
        if (!callerAllowed() || (calledFromDBus() && message().service() != m_probeCaller)) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (lease <= 0 || static_cast<quint64>(lease) != m_probeLease || m_probePhase >= 3) {
            return errJson(QStringLiteral("probe_expired"));
        }
        if (!accessibilityProbeOwned()) {
            finishAccessibilityProbe();
            return errJson(QStringLiteral("focus_changed_during_probe"));
        }
        ++m_probePhase;
        // requestFocus omits ActivityRaise. Repeat leave/enter once to reject
        // delayed activation events. No key, pointer or click is sent.
        bool focused = true;
        if (m_probePhase % 2) {
            focused = m_probeTarget && KWin::workspace()->requestFocus(m_probeTarget, true);
        } else {
            KWin::workspace()->activateWindow(nullptr);
        }
        m_probeEpoch = m_focusEpoch;
        if (!focused || !accessibilityProbeOwned()) {
            finishAccessibilityProbe();
            return errJson(QStringLiteral("focus_not_confirmed"));
        }
        QJsonObject object;
        object["ok"] = true;
        object["epoch"] = static_cast<qint64>(m_probeEpoch);
        return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString EndAccessibilityProbe(qint64 lease)
    {
        if (!callerAllowed() || (calledFromDBus() && message().service() != m_probeCaller)) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (lease <= 0 || static_cast<quint64>(lease) != m_probeLease) {
            return errJson(QStringLiteral("probe_expired"));
        }
        const bool stable = finishAccessibilityProbe();
        QJsonObject object;
        object["ok"] = stable;
        object["reason"] = stable ? "" : "focus_changed_during_probe";
        return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString GetWindows()
    {
        const auto liveWindows = KWin::Workspace::self()->windows();
        QSet<QUuid> liveIds;
        for (KWin::Window *window : liveWindows) {
            if (isUserSurface(window)) {
                liveIds.insert(window->internalId());
            }
        }
        for (auto it = m_tokens.begin(); it != m_tokens.end();) {
            if (!liveIds.contains(it.key())) {
                it = m_tokens.erase(it);
            } else {
                ++it;
            }
        }

        QJsonArray windows;
        for (KWin::Window *window : liveWindows) {
            if (!isUserSurface(window)) {
                continue;
            }
            const auto frame = window->frameGeometry();
            QJsonObject record;
            record["token"] = static_cast<qint64>(tokenFor(window->internalId()));
            record["handle"] = window->internalId().toString();
            record["pid"] = window->pid();
            record["title"] = window->captionNormal();
            record["app_id"] = window->resourceClass();
            record["x"] = qRound(frame.x());
            record["y"] = qRound(frame.y());
            record["w"] = qRound(frame.width());
            record["h"] = qRound(frame.height());
            record["active"] = window->isActive();
            record["minimized"] = window->isMinimized();
            record["stacking"] = window->stackingOrder();
            auto *parent = window->transientFor();
            record["transient_for"] = parent && isUserSurface(parent)
                ? QJsonValue(static_cast<qint64>(tokenFor(parent->internalId()))) : QJsonValue();
            record["modal"] = window->isModal();
            record["window_type"] = isUserPopup(window)
                ? (window->isTooltip() ? "tooltip" : "popup")
                : (window->isDialog() ? "dialog" : "normal");
            windows.push_back(record);
        }
        return QString::fromUtf8(QJsonDocument(windows).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString GetScreenGeometry()
    {
        const KWin::Rect geometry = KWin::effects->virtualScreenGeometry();
        QJsonObject object;
        object[QStringLiteral("x")] = geometry.x();
        object[QStringLiteral("y")] = geometry.y();
        object[QStringLiteral("width")] = geometry.width();
        object[QStringLiteral("height")] = geometry.height();
        QJsonArray outputs;
        for (KWin::LogicalOutput *output : KWin::effects->screens()) {
            const KWin::Rect frame = output->geometry();
            QJsonObject record;
            record[QStringLiteral("name")] = output->name();
            record[QStringLiteral("x")] = frame.x();
            record[QStringLiteral("y")] = frame.y();
            record[QStringLiteral("width")] = frame.width();
            record[QStringLiteral("height")] = frame.height();
            record[QStringLiteral("scale")] = output->scale();
            outputs.append(record);
        }
        object[QStringLiteral("outputs")] = outputs;
        return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString SetWindowFrame(qint64 token, int x, int y, int width, int height)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        if (width <= 0 || height <= 0) {
            return errJson(QStringLiteral("invalid_size"));
        }
        window->moveResize(KWin::RectF(x, y, width, height));
        return okJson();
    }

    // Press and hold a pointer button over one exact window without raising it.
    // The press may still make KWin's activation policy focus the target; the
    // previous active window is restored when the button is released.
    Q_SCRIPTABLE QString MouseButtonDownBackground(qint64 token, int x, int y, uint button)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        if (m_holdActive) {
            return errJson(QStringLiteral("button_already_down"));
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (window->isMinimized()) {
            return errJson(QStringLiteral("target_minimized"));
        }
        m_holdRestorePointer = KWin::input()->globalPointer();
        m_holdPreviousActive = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        m_holdStacking = KWin::workspace() ? KWin::workspace()->stackingOrder() : QList<KWin::Window *>();
        if (KWin::workspace()) {
            KWin::workspace()->raiseWindow(window, true);
            m_holdRaised = true;
        }
        auto *pointer = KWin::input()->pointer();
        movePointerToPoint(point);
        if (!pointerOverTarget(window, point, &error)) {
            KWin::input()->warpPointer(m_holdRestorePointer);
            flushPointerFrame();
            if (m_holdPreviousActive && KWin::workspace()
                && KWin::workspace()->activeWindow() != m_holdPreviousActive) {
                KWin::workspace()->activateWindow(m_holdPreviousActive, true);
            }
            m_holdPreviousActive = nullptr;
            if (m_holdRaised) {
                restoreStacking(m_holdStacking);
                m_holdRaised = false;
            }
            m_holdStacking.clear();
            return errJson(error);
        }
        pointer->processButton(button, KWin::PointerButtonState::Pressed, nowUs(), nullptr);
        flushPointerFrame();
        m_holdActive = true;
        m_holdToken = token;
        m_holdButton = button;
        return okJson();
    }

    // Move the held pointer over the same target. The pointer stays at the new
    // point so the client sees one continuous drag.
    Q_SCRIPTABLE QString MouseMoveBackground(qint64 token, int x, int y)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        if (!m_holdActive || m_holdToken != token) {
            return errJson(QStringLiteral("no_held_button"));
        }
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        auto *pointer = KWin::input()->pointer();
        movePointerToPoint(point);
        if (!pointerOverTarget(window, point, &error)) {
            return errJson(error);
        }
        return okJson();
    }

    Q_SCRIPTABLE QString MouseButtonUpBackground(qint64 token, int x, int y)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        if (!m_holdActive || m_holdToken != token) {
            return errJson(QStringLiteral("no_held_button"));
        }
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF point(x, y);
        const bool inside = window->frameGeometry().contains(point);
        auto *pointer = KWin::input()->pointer();
        if (inside) {
            pointer->processMotionAbsolute(point, nowUs(), nullptr);
            flushPointerFrame();
            pointerOverTarget(window, point, &error);
        } else {
            error = QStringLiteral("point_outside_target");
        }
        // Release even when the final point is no longer over the target so a
        // failed drag cannot leave the user's pointer button stuck down.
        pointer->processButton(m_holdButton, KWin::PointerButtonState::Released, nowUs(), nullptr);
        flushPointerFrame();
        KWin::input()->warpPointer(m_holdRestorePointer);
        flushPointerFrame();
        m_holdActive = false;
        m_holdToken = 0;
        m_holdButton = 0;
        if (m_holdPreviousActive && KWin::workspace()
            && KWin::workspace()->activeWindow() != m_holdPreviousActive) {
            KWin::workspace()->activateWindow(m_holdPreviousActive, true);
        }
        m_holdPreviousActive = nullptr;
        if (m_holdRaised) {
            restoreStacking(m_holdStacking);
            m_holdRaised = false;
        }
        m_holdStacking.clear();
        if (!error.isEmpty()) {
            return errJson(error);
        }
        return okJson();
    }

    Q_SCRIPTABLE qint64 GetActiveToken()
    {
        KWin::Window *window = KWin::Workspace::self()->activeWindow();
        if (!window || !window->isClient() || window->isDeleted()) {
            return 0;
        }
        return static_cast<qint64>(tokenFor(window->internalId()));
    }

    Q_SCRIPTABLE QString Activate(qint64 token)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        return okJson();
    }

    Q_SCRIPTABLE QString Move(qint64 token, int x, int y)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *pointer = KWin::input()->pointer();
        movePointerToPoint(point);
        if (!pointerOverTarget(window, point, &error)) {
            return errJson(error);
        }
        return okJson();
    }

    Q_SCRIPTABLE QString Click(qint64 token, int x, int y, uint button, int count)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token, true);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        // xdg_popup surfaces cannot be activated as toplevels. Their existing
        // grab and exact pointer surface are sufficient for a targeted click.
        if (isUserPopup(window)) {
            return ClickBackground(token, x, y, button, count);
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *pointer = KWin::input()->pointer();
        movePointerToPoint(point);
        if (!pointerOverTarget(window, point, &error)) {
            return errJson(error);
        }
        const int clicks = qBound(1, count, 3);
        for (int i = 0; i < clicks; ++i) {
            pointer->processButton(button, KWin::PointerButtonState::Pressed, nowUs(), nullptr);
            flushPointerFrame();
            QThread::msleep(8);
            pointer->processButton(button, KWin::PointerButtonState::Released, nowUs(), nullptr);
            flushPointerFrame();
            if (i + 1 < clicks) {
                QThread::msleep(70);
            }
        }
        return okJson();
    }

    Q_SCRIPTABLE QString Scroll(qint64 token, int x, int y, double dx, double dy)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *pointer = KWin::input()->pointer();
        pointer->processMotionAbsolute(point, nowUs(), nullptr);
        if (!pointerOverTarget(window, point, &error)) {
            return errJson(error);
        }
        if (!qFuzzyIsNull(dy)) {
            pointer->processAxis(KWin::PointerAxis::Vertical,
                                 dy * 15.0,
                                 qRound(dy * 120.0),
                                 KWin::PointerAxisSource::Wheel,
                                 false,
                                 false,
                                 nowUs(),
                                 nullptr);
        }
        if (!qFuzzyIsNull(dx)) {
            pointer->processAxis(KWin::PointerAxis::Horizontal,
                                 dx * 15.0,
                                 qRound(dx * 120.0),
                                 KWin::PointerAxisSource::Wheel,
                                 false,
                                 false,
                                 nowUs(),
                                 nullptr);
        }
        flushPointerFrame();
        return okJson();
    }

    Q_SCRIPTABLE QString KeySequence(qint64 token, const QString &json)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        const QJsonDocument document = QJsonDocument::fromJson(json.toUtf8());
        if (!document.isArray()) {
            return errJson(QStringLiteral("invalid_sequence"));
        }
        const QJsonArray transitions = document.array();
        if (transitions.isEmpty() || transitions.size() > 64) {
            return errJson(QStringLiteral("invalid_sequence"));
        }
        // Validate before activation or any key is sent.
        for (const QJsonValue &value : transitions) {
            const int code = value.toObject().value(QStringLiteral("code")).toInt(-1);
            if (code <= 0 || code > 0x2ff) {
                return errJson(QStringLiteral("invalid_keycode"));
            }
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        m_inputBusy = true;
        if (!activate(window, &error)) {
            m_inputBusy = false;
            return errJson(error);
        }
        auto *keyboard = KWin::input()->keyboard();
        if (m_keyboardOwner != keyboard) {
            m_ownedKeys.clear();
        }
        m_keyboardOwner = keyboard;
        int focusDelay = 0;
        const int settleMs = window->resourceClass().toLower().contains(QStringLiteral("konsole")) ? 300 : 100;
        if (m_keyFocusId == window->internalId() && m_keyFocusTimer.isValid()) {
            // Wait only for the remainder of the toolkit's focus-in interval.
            // Konsole needs longer before its paste shortcut is ready.
            focusDelay = qMax(0, settleMs - static_cast<int>(qMin<qint64>(settleMs, m_keyFocusTimer.elapsed())));
        }
        const auto deliverOne = [this, token](const QJsonValue &value) {
            const QJsonObject transition = value.toObject();
            const bool pressed = transition.value(QStringLiteral("pressed")).toBool();
            const uint code = transition.value(QStringLiteral("code")).toInt();
            if (pressed) {
                KWin::Window *target = windowForToken(token);
                if (!target || target->isDeleted()) {
                    return QStringLiteral("target_gone");
                }
                if (!target->isActive()) {
                    return QStringLiteral("target_focus_changed");
                }
            }
            if (!m_keyboardOwner) {
                return QStringLiteral("keyboard_unavailable");
            }
            const bool wasPressed = m_keyboardOwner->pressedKeys().contains(code);
            // Releases must complete even when a shortcut closed its target.
            m_keyboardOwner->processKey(code, pressed ? KWin::KeyboardKeyState::Pressed
                                                      : KWin::KeyboardKeyState::Released,
                                        nowUs(), nullptr);
            if (pressed && !wasPressed) {
                m_ownedKeys.insert(code);
            } else if (!pressed) {
                m_ownedKeys.remove(code);
            }
            return QString();
        };
        if (calledFromDBus()) {
            setDelayedReply(true);
            const auto request = message();
            const auto bus = connection();
            const auto initiallyPressed = KWin::input()->keyboard()->pressedKeys();
            auto *timer = new QTimer(this);
            timer->setSingleShot(true);
            timer->setTimerType(Qt::PreciseTimer);
            connect(timer, &QTimer::timeout, this,
                    [this, timer, token, transitions, deliverOne, initiallyPressed,
                     bus, request, index = 0, held = QSet<uint32_t>()]() mutable {
                if (index == transitions.size()) {
                    timer->deleteLater();
                    QTimer::singleShot(100, this, [this, bus, request]() {
                        m_inputBusy = false;
                        bus.send(request.createReply(QVariantList{okJson()}));
                    });
                    return;
                }
                const QString error = deliverOne(transitions.at(index));
                if (!error.isEmpty()) {
                    // Release only keys pressed by this request if focus or
                    // the target disappeared between transitions.
                    for (uint32_t code : std::as_const(held)) {
                        if (m_keyboardOwner) {
                            m_keyboardOwner->processKey(code, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
                        }
                        m_ownedKeys.remove(code);
                    }
                    timer->deleteLater();
                    m_inputBusy = false;
                    bus.send(request.createReply(QVariantList{errJson(error)}));
                    return;
                }
                const QJsonObject transition = transitions.at(index).toObject();
                const uint code = transition.value(QStringLiteral("code")).toInt();
                if (transition.value(QStringLiteral("pressed")).toBool()) {
                    if (!initiallyPressed.contains(code) && m_ownedKeys.contains(code)) {
                        held.insert(code);
                    }
                } else {
                    held.remove(code);
                }
                ++index;
                // Yield to KWin's event loop so focus and modifier updates
                // reach the client before the next key transition.
                timer->start(10);
            });
            timer->start(focusDelay);
            return {};
        }
        const auto initiallyPressed = keyboard->pressedKeys();
        for (const QJsonValue &value : transitions) {
            const QString error = deliverOne(value);
            if (!error.isEmpty()) {
                const auto owned = m_ownedKeys;
                for (uint32_t code : owned) {
                    if (!initiallyPressed.contains(code) && m_keyboardOwner) {
                        m_keyboardOwner->processKey(code, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
                        m_ownedKeys.remove(code);
                    }
                }
                m_inputBusy = false;
                return errJson(error);
            }
            keyGap();
        }
        m_inputBusy = false;
        return okJson();
    }

    Q_SCRIPTABLE QString PressKey(qint64 token, uint keycode)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        if (keycode == 0 || keycode > 0x2ff) {
            return errJson(QStringLiteral("invalid_keycode"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *keyboard = KWin::input()->keyboard();
        keyboard->processKey(keycode, KWin::KeyboardKeyState::Pressed, nowUs(), nullptr);
        keyGap();
        keyboard->processKey(keycode, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
        keyGap();
        return okJson();
    }

    Q_SCRIPTABLE QString TypeText(qint64 token, const QString &text)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        if (text.isEmpty()) {
            return okJson();
        }
        if (text.size() > 4096) {
            return errJson(QStringLiteral("text_too_large"));
        }
        QVector<KeyStroke> strokes;
        strokes.reserve(text.size());
        for (const QChar character : text) {
            KeyStroke stroke{};
            if (!asciiStroke(character, &stroke)) {
                return errJson(QStringLiteral("unsupported_character"));
            }
            strokes.push_back(stroke);
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *keyboard = KWin::input()->keyboard();
        for (const KeyStroke &stroke : strokes) {
            if (stroke.shift) {
                keyboard->processKey(kLeftShift, KWin::KeyboardKeyState::Pressed, nowUs(), nullptr);
                keyGap();
            }
            keyboard->processKey(stroke.code, KWin::KeyboardKeyState::Pressed, nowUs(), nullptr);
            keyGap();
            keyboard->processKey(stroke.code, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
            keyGap();
            if (stroke.shift) {
                keyboard->processKey(kLeftShift, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
                keyGap();
            }
        }
        return okJson();
    }

    Q_SCRIPTABLE QString Drag(qint64 token, int x1, int y1, int x2, int y2, uint button)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF start(x1, y1);
        const QPointF end(x2, y2);
        const auto frame = window->frameGeometry();
        if (!frame.contains(start) || !frame.contains(end)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *pointer = KWin::input()->pointer();
        pointer->processMotionAbsolute(start, nowUs(), nullptr);
        if (!pointerOverTarget(window, start, &error)) {
            return errJson(error);
        }
        pointer->processButton(button, KWin::PointerButtonState::Pressed, nowUs(), nullptr);
        constexpr int kSteps = 20;
        for (int step = 1; step <= kSteps; ++step) {
            const qreal progress = static_cast<qreal>(step) / kSteps;
            const QPointF point = start + (end - start) * progress;
            pointer->processMotionAbsolute(point, nowUs(), nullptr);
            flushPointerFrame();
            QThread::msleep(10);
        }
        pointer->processButton(button, KWin::PointerButtonState::Released, nowUs(), nullptr);
        return okJson();
    }

    Q_SCRIPTABLE QString GetPointer()
    {
        const QPointF point = KWin::input()->globalPointer();
        QJsonObject object;
        object[QStringLiteral("x")] = point.x();
        object[QStringLiteral("y")] = point.y();
        return QString::fromUtf8(QJsonDocument(object).toJson(QJsonDocument::Compact));
    }

    Q_SCRIPTABLE QString MovePointer(double x, double y)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        KWin::input()->warpPointer(QPointF(x, y));
        flushPointerFrame();
        return okJson();
    }

    Q_SCRIPTABLE QString ClickBackground(qint64 token, int x, int y, uint button, int count)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token, true);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        if (isUserPopup(window) && count != 1) {
            return errJson(QStringLiteral("popup_requires_single_click"));
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (window->isMinimized()) {
            return errJson(QStringLiteral("target_minimized"));
        }
        const QPointF saved = KWin::input()->globalPointer();
        KWin::Window *previous = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        StackingGuard stackingGuard(window, !isUserPopup(window));
        if (isUserPopup(window)) stackingGuard.disarm();
        ActiveWindowGuard activeGuard(previous);
        auto *pointer = KWin::input()->pointer();
        movePointerToPoint(point);
        if (!pointerOverTarget(window, point, &error)) {
            KWin::input()->warpPointer(saved);
            flushPointerFrame();
            return errJson(error);
        }
        const int clicks = qBound(1, count, 3);
        for (int i = 0; i < clicks; ++i) {
            pointer->processButton(button, KWin::PointerButtonState::Pressed, nowUs(), nullptr);
            flushPointerFrame();
            QThread::msleep(8);
            pointer->processButton(button, KWin::PointerButtonState::Released, nowUs(), nullptr);
            flushPointerFrame();
            if (i + 1 < clicks) {
                QThread::msleep(70);
            }
        }
        KWin::input()->warpPointer(saved);
        flushPointerFrame();
        return okJson();
    }

    Q_SCRIPTABLE QString ScrollBackground(qint64 token, int x, int y, double dx, double dy)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF point(x, y);
        if (!window->frameGeometry().contains(point)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (window->isMinimized()) {
            return errJson(QStringLiteral("target_minimized"));
        }
        const QPointF saved = KWin::input()->globalPointer();
        KWin::Window *previous = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        // Activation raises the target as part of KWin's own focus path. Do
        // not pre-raise it here: the compositor's pointer/axis path behaved
        // differently when a foreign raise preceded activation, while the
        // normal activate-then-scroll path is the one the foreground method
        // already proves. Snapshot only, then restore after delivery.
        StackingGuard stackingGuard(window, false);
        QList<QPointer<KWin::Window>> order;
        for (KWin::Window *item : stackingGuard.order()) {
            order.push_back(QPointer<KWin::Window>(item));
        }
        const QPointer<KWin::Window> previousSafe(previous);
        ActiveWindowGuard activeGuard(previous);
        // KWin routes wheel events through the active window; transiently
        // activate the target so the axis lands in the intended client.
        m_inputBusy = true;
        if (!activate(window, &error)) {
            m_inputBusy = false;
            return errJson(error);
        }
        auto *pointer = KWin::input()->pointer();
        // Match the foreground scroll path exactly: one motion, no extra
        // pre-wheel frame. The approach motion is useful for clicks, but it
        // made the occluded wheel path diverge from the proven method.
        pointer->processMotionAbsolute(point, nowUs(), nullptr);
        if (!pointerOverTarget(window, point, &error)) {
            KWin::input()->warpPointer(saved);
            flushPointerFrame();
            m_inputBusy = false;
            return errJson(error);
        }
        if (!qFuzzyIsNull(dy)) {
            pointer->processAxis(KWin::PointerAxis::Vertical,
                                 dy * 15.0,
                                 qRound(dy * 120.0),
                                 KWin::PointerAxisSource::Wheel,
                                 false,
                                 false,
                                 nowUs(),
                                 nullptr);
        }
        if (!qFuzzyIsNull(dx)) {
            pointer->processAxis(KWin::PointerAxis::Horizontal,
                                 dx * 15.0,
                                 qRound(dx * 120.0),
                                 KWin::PointerAxisSource::Wheel,
                                 false,
                                 false,
                                 nowUs(),
                                 nullptr);
        }
        flushPointerFrame();
        // The wheel reaches the client through KWin's event loop, so restoring
        // focus or stacking synchronously in this effect callback can discard
        // it before the client sees it. Keep the target active and raised for
        // one delivery window, then restore from the event loop. Sleeping here
        // would block the same event loop and make the loss worse.
        stackingGuard.disarm();
        activeGuard.disarm();
        const auto restore = [this, previousSafe, order, saved]() {
            auto *workspace = KWin::workspace();
            if (previousSafe && !previousSafe->isDeleted() && workspace
                && workspace->activeWindow() != previousSafe.data()) {
                workspace->activateWindow(previousSafe.data(), true);
            }
            KWin::input()->warpPointer(saved);
            flushPointerFrame();
            if (workspace) {
                for (const auto &item : order) {
                    if (item && !item->isDeleted() && !item->isMinimized()
                        && isUserToplevel(item.data())) {
                        workspace->raiseWindow(item.data(), true);
                    }
                }
            }
            KWin::input()->pointer()->processMotionAbsolute(saved, nowUs(), nullptr);
            flushPointerFrame();
            m_inputBusy = false;
        };
        // A following batch step must start after this action restores focus.
        if (calledFromDBus()) {
            setDelayedReply(true);
            const auto request = message();
            const auto bus = connection();
            QTimer::singleShot(150, this, [restore, bus, request]() {
                restore();
                bus.send(request.createReply(QVariantList{okJson()}));
            });
            return {};
        }
        QTimer::singleShot(150, this, restore);
        return okJson();
    }

    Q_SCRIPTABLE QString DragBackground(qint64 token, int x1, int y1, int x2, int y2, uint button)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        QString error;
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        const QPointF start(x1, y1);
        const QPointF end(x2, y2);
        const auto frame = window->frameGeometry();
        if (!frame.contains(start) || !frame.contains(end)) {
            return errJson(QStringLiteral("point_outside_target"));
        }
        if (window->isMinimized()) {
            return errJson(QStringLiteral("target_minimized"));
        }
        const QPointF saved = KWin::input()->globalPointer();
        KWin::Window *previous = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        StackingGuard stackingGuard(window);
        ActiveWindowGuard activeGuard(previous);
        auto *pointer = KWin::input()->pointer();
        movePointerToPoint(start);
        if (!pointerOverTarget(window, start, &error)) {
            KWin::input()->warpPointer(saved);
            flushPointerFrame();
            return errJson(error);
        }
        pointer->processButton(button, KWin::PointerButtonState::Pressed, nowUs(), nullptr);
        constexpr int kSteps = 20;
        for (int step = 1; step <= kSteps; ++step) {
            const qreal progress = static_cast<qreal>(step) / kSteps;
            const QPointF point = start + (end - start) * progress;
            pointer->processMotionAbsolute(point, nowUs(), nullptr);
            flushPointerFrame();
            QThread::msleep(10);
        }
        pointer->processButton(button, KWin::PointerButtonState::Released, nowUs(), nullptr);
        flushPointerFrame();
        KWin::input()->warpPointer(saved);
        flushPointerFrame();
        return okJson();
    }

    Q_SCRIPTABLE QString KeySequenceBackground(qint64 token, const QString &json)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        const QJsonDocument document = QJsonDocument::fromJson(json.toUtf8());
        if (!document.isArray()) {
            return errJson(QStringLiteral("invalid_sequence"));
        }
        const QJsonArray transitions = document.array();
        if (transitions.isEmpty() || transitions.size() > 64) {
            return errJson(QStringLiteral("invalid_sequence"));
        }
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        KWin::Window *previous = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        StackingGuard stackingGuard(window, false);
        ActiveWindowGuard activeGuard(previous);
        QString error;
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *keyboard = KWin::input()->keyboard();
        for (const QJsonValue &value : transitions) {
            const QJsonObject transition = value.toObject();
            const int code = transition.value(QStringLiteral("code")).toInt(-1);
            if (code <= 0 || code > 0x2ff) {
                return errJson(QStringLiteral("invalid_keycode"));
            }
            const bool pressed = transition.value(QStringLiteral("pressed")).toBool();
            keyboard->processKey(static_cast<uint32_t>(code),
                                 pressed ? KWin::KeyboardKeyState::Pressed
                                         : KWin::KeyboardKeyState::Released,
                                 nowUs(),
                                 nullptr);
            keyGap();
        }
        // A chord may be consumed by KWin's global-shortcut path and the
        // target toolkit asynchronously. Restoring focus too soon can drop
        // Ctrl/Alt chords while leaving them applied to the previous window.
        // Hold the target for a delivery window, then restore on the event
        // loop (sleeping here would block that same dispatch).
        const QList<KWin::Window *> order = stackingGuard.order();
        stackingGuard.disarm();
        activeGuard.disarm();
        QTimer::singleShot(300, this, [previous, order]() {
            if (previous && KWin::workspace()
                && KWin::workspace()->activeWindow() != previous) {
                KWin::workspace()->activateWindow(previous, true);
            }
            restoreStacking(order);
        });
        return okJson();
    }

    Q_SCRIPTABLE QString PressKeyBackground(qint64 token, uint keycode)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        if (keycode == 0 || keycode > 0x2ff) {
            return errJson(QStringLiteral("invalid_keycode"));
        }
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        KWin::Window *previous = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        StackingGuard stackingGuard(window, false);
        ActiveWindowGuard activeGuard(previous);
        QString error;
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *keyboard = KWin::input()->keyboard();
        keyboard->processKey(keycode, KWin::KeyboardKeyState::Pressed, nowUs(), nullptr);
        keyGap();
        keyboard->processKey(keycode, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
        keyGap();
        return okJson();
    }

    Q_SCRIPTABLE QString TypeTextBackground(qint64 token, const QString &text)
    {
        if (!callerAllowed()) {
            return errJson(QStringLiteral("not_authorized"));
        }
        if (m_inputBusy) {
            return errJson(QStringLiteral("input_busy"));
        }
        if (text.isEmpty()) {
            return okJson();
        }
        if (text.size() > 4096) {
            return errJson(QStringLiteral("text_too_large"));
        }
        QVector<KeyStroke> strokes;
        strokes.reserve(text.size());
        for (const QChar character : text) {
            KeyStroke stroke{};
            if (!asciiStroke(character, &stroke)) {
                return errJson(QStringLiteral("unsupported_character"));
            }
            strokes.push_back(stroke);
        }
        KWin::Window *window = windowForToken(token);
        if (!window) {
            return errJson(QStringLiteral("unknown_token"));
        }
        KWin::Window *previous = KWin::workspace() ? KWin::workspace()->activeWindow() : nullptr;
        StackingGuard stackingGuard(window, false);
        ActiveWindowGuard activeGuard(previous);
        QString error;
        if (!activate(window, &error)) {
            return errJson(error);
        }
        auto *keyboard = KWin::input()->keyboard();
        for (const KeyStroke &stroke : strokes) {
            if (stroke.shift) {
                keyboard->processKey(kLeftShift, KWin::KeyboardKeyState::Pressed, nowUs(), nullptr);
                keyGap();
            }
            keyboard->processKey(stroke.code, KWin::KeyboardKeyState::Pressed, nowUs(), nullptr);
            keyGap();
            keyboard->processKey(stroke.code, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
            keyGap();
            if (stroke.shift) {
                keyboard->processKey(kLeftShift, KWin::KeyboardKeyState::Released, nowUs(), nullptr);
                keyGap();
            }
        }
        return okJson();
    }

private:
    qint64 keyboardFocusToken()
    {
        auto *surface = KWin::waylandServer()->seat()->focusedKeyboardSurface();
        if (!surface) {
            return 0;
        }
        surface = surface->mainSurface();
        for (auto *window : KWin::workspace()->windows()) {
            if (window && !window->isDeleted() && window->surface() == surface) {
                return static_cast<qint64>(tokenFor(window->internalId()));
            }
        }
        return 0;
    }

    bool accessibilityProbeOwned()
    {
        if (!m_probeLease || m_focusEpoch != m_probeEpoch) {
            return false;
        }
        if (m_probePhase % 2) {
            return m_probeTarget && !m_probeTarget->isDeleted()
                && KWin::workspace()->activeWindow() == m_probeTarget
                && keyboardFocusToken() == static_cast<qint64>(tokenFor(m_probeTarget->internalId()));
        }
        return !KWin::workspace()->activeWindow() && keyboardFocusToken() == 0;
    }

    bool finishAccessibilityProbe()
    {
        if (!m_probeLease) {
            return false;
        }
        const bool owned = accessibilityProbeOwned();
        const bool complete = m_probePhase == 3;
        // A user's focus change supersedes the borrowed focus. Never steal it
        // back or restack windows after that change.
        if (owned) {
            QList<KWin::Window *> order;
            for (const auto &window : std::as_const(m_probeOrder)) {
                if (window && !window->isDeleted()) {
                    order.push_back(window);
                }
            }
            if (order != KWin::workspace()->stackingOrder()) {
                restoreStacking(order);
            }
            if (m_probePrevious && !m_probePrevious->isDeleted()) {
                KWin::workspace()->requestFocus(m_probePrevious, true);
            } else {
                KWin::workspace()->activateWindow(nullptr);
            }
        }
        const bool restored = KWin::workspace()->activeWindow() == m_probePrevious
            && keyboardFocusToken() == m_probePreviousKeyboard;
        m_probeLease = 0;
        m_probeTarget = nullptr;
        m_probePrevious = nullptr;
        m_probeOrder.clear();
        m_probeCaller.clear();
        m_inputBusy = false;
        return owned && complete && restored;
    }

    bool callerAllowed()
    {
        if (!calledFromDBus()) {
            return true;
        }
        const QString sender = message().service();
        if (sender.isEmpty()) {
            return false;
        }
        auto *busInterface = QDBusConnection::sessionBus().interface();
        if (!busInterface) {
            return false;
        }
        const uint callerUid = busInterface->serviceUid(sender);
        return callerUid == static_cast<uint>(::getuid());
    }

    quint64 tokenFor(const QUuid &id)
    {
        if (const auto token = m_tokens.constFind(id); token != m_tokens.cend()) {
            return *token;
        }
        const quint64 token = m_nextToken++;
        m_tokens.insert(id, token);
        return token;
    }

    KWin::Window *windowForToken(qint64 token, bool allowPopup = false)
    {
        if (token <= 0) {
            return nullptr;
        }
        KWin::Workspace *workspace = KWin::Workspace::self();
        if (!workspace) {
            return nullptr;
        }
        KWin::Window *found = nullptr;
        QSet<QUuid> liveIds;
        for (KWin::Window *window : workspace->windows()) {
            if (!window || window->isDeleted()) {
                continue;
            }
            liveIds.insert(window->internalId());
            if (!isUserToplevel(window) && !(allowPopup && isUserPopup(window))) {
                continue;
            }
            const auto it = m_tokens.constFind(window->internalId());
            if (it != m_tokens.cend() && static_cast<qint64>(*it) == token) {
                found = window;
            }
        }
        for (auto it = m_tokens.begin(); it != m_tokens.end();) {
            if (!liveIds.contains(it.key())) {
                it = m_tokens.erase(it);
            } else {
                ++it;
            }
        }
        return found;
    }

    bool activate(KWin::Window *window, QString *error)
    {
        if (!window || window->isDeleted()) {
            *error = QStringLiteral("target_gone");
            return false;
        }
        const bool changingFocus = !window->isActive();
        KWin::Workspace::self()->activateWindow(window, true);
        QElapsedTimer timer;
        timer.start();
        while (!window->isActive() && timer.elapsed() < 200) {
            QCoreApplication::processEvents(QEventLoop::ExcludeUserInputEvents, 5);
            QThread::msleep(1);
        }
        if (!window->isActive()) {
            *error = QStringLiteral("activation_not_confirmed");
            return false;
        }
        if (changingFocus) {
            m_keyFocusId = window->internalId();
            m_keyFocusTimer.start();
        }
        return true;
    }

    bool pointerOverTarget(KWin::Window *window, const QPointF &point, QString *error)
    {
        KWin::Window *under = KWin::input()->findToplevel(point);
        if (under != window) {
            *error = QStringLiteral("pointer_not_over_target");
            return false;
        }
        return true;
    }

    static void restoreStacking(const QList<KWin::Window *> &order)
    {
        auto *workspace = KWin::workspace();
        if (!workspace) {
            return;
        }
        for (KWin::Window *window : std::as_const(order)) {
            if (!window || window->isDeleted() || window->isMinimized()
                || !isUserToplevel(window)) {
                continue;
            }
            workspace->raiseWindow(window, true);
        }
        const QPointF current = KWin::input()->globalPointer();
        KWin::input()->pointer()->processMotionAbsolute(current, nowUs(), nullptr);
        flushPointerFrame();
    }

    bool m_inputBusy = false;
    quint64 m_focusEpoch = 1;
    quint64 m_probeEpoch = 0;
    quint64 m_probeLease = 0;
    quint64 m_nextProbeLease = 0;
    unsigned int m_probePhase = 0;
    qint64 m_probePreviousKeyboard = 0;
    QString m_probeCaller;
    QPointer<KWin::Window> m_probePrevious;
    QPointer<KWin::Window> m_probeTarget;
    QList<QPointer<KWin::Window>> m_probeOrder;
    QPointer<KWin::KeyboardInputRedirection> m_keyboardOwner;
    QSet<uint32_t> m_ownedKeys;
    QUuid m_keyFocusId;
    QElapsedTimer m_keyFocusTimer;
    QHash<QUuid, quint64> m_tokens;
    quint64 m_nextToken = 1;
    bool m_holdActive = false;
    qint64 m_holdToken = 0;
    uint m_holdButton = 0;
    QPointF m_holdRestorePointer;
    KWin::Window *m_holdPreviousActive = nullptr;
    QList<KWin::Window *> m_holdStacking;
    bool m_holdRaised = false;
};

class KwinTargetHelperEffect : public KWin::Effect
{
public:
    KwinTargetHelperEffect()
    {
        auto bus = QDBusConnection::sessionBus();
        if (!bus.registerService(QString::fromLatin1(kService))) {
            return;
        }
        m_exported = bus.registerObject(
            QString::fromLatin1(kPath),
            &m_api,
            QDBusConnection::ExportScriptableSlots);
        if (!m_exported) {
            bus.unregisterService(QString::fromLatin1(kService));
        }
    }

    ~KwinTargetHelperEffect() override
    {
        if (!m_exported) {
            return;
        }
        auto bus = QDBusConnection::sessionBus();
        bus.unregisterObject(QString::fromLatin1(kPath));
        bus.unregisterService(QString::fromLatin1(kService));
    }

private:
    TargetApi m_api;
    bool m_exported = false;
};
}

KWIN_EFFECT_FACTORY(KwinTargetHelperEffect, "kwin_target_helper.json")

#include "kwin_target_helper.moc"
