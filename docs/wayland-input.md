# Wayland automation and physical-input pause

[Visor #17](https://github.com/hyperme-sh/visor/issues/17) covers native input,
physical-input pause, local consent, and the automation overlay on GNOME and KDE.
Visor currently allows Wayland screenshot and focus queries. Automation start
and resume, pointer input, and typing return 501. The Linux backend also refuses
direct and batched input when both Wayland and XWayland are present.

## Available compositor interfaces

| Interface | What Visor can use it for |
| --- | --- |
| [RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) | After local consent and session start, `ConnectToEIS` supplies a sender for keyboard and pointer input. This interface provides no notification of physical input. |
| [InputCapture portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html) | The compositor activates capture after a pointer crosses a screen-edge barrier. Active capture diverts events to the capture client. A key press or mouse movement in the middle of the screen cannot trigger the pause Visor needs during normal automation. |
| [KWin idle notification](https://github.com/KDE/kwin/blob/f1e7d1d4430586d818d9d893f40b157b0f682dbd/src/wayland/idlenotify_v1.cpp#L52-L71) | The input-specific request ignores idle inhibitors and uses the same activity detector. [EIS input](https://github.com/KDE/kwin/blob/f1e7d1d4430586d818d9d893f40b157b0f682dbd/src/plugins/eis/eiscontext.cpp#L259-L296) enters the input event path, and the [activity observer](https://github.com/KDE/kwin/blob/f1e7d1d4430586d818d9d893f40b157b0f682dbd/src/input.cpp#L3031-L3053) counts button and key events without checking their source. An idle transition cannot reliably identify local input. |
| [Mutter IdleMonitor](https://github.com/GNOME/mutter/blob/30405578e2c9bab5b3d1af4ab31bd8d3cea81615/data/dbus-interfaces/org.gnome.Mutter.IdleMonitor.xml) | The core monitor exposes idle duration and watch IDs. Active watches are one-shot and carry no device or input source. [Mutter can also reset idle time on lid and power changes](https://github.com/GNOME/mutter/blob/30405578e2c9bab5b3d1af4ab31bd8d3cea81615/src/backends/meta-backend.c#L624-L673). |

These interfaces supply injection and aggregate activity. The missing
compositor capability is a session-scoped interruption signal that detects
physical input while the desktop continues to handle it normally. XInput2 on
XWayland only covers X11 events and cannot classify the portal sender using
the XTEST device filter. Suppressing notifications while Visor injects would
also suppress a user's interruption during that interval.

## What an input backend must establish

- Local pointer motion, button presses, and key presses pause automation
  even during a burst from Visor's EIS sender or while a native Wayland client
  has focus. Emulated input from the sender does not trigger that pause.
- The compositor can signal interruption without delivering raw key or button
  values to Visor. The signal belongs to the consented seat and RemoteDesktop
  session.
- A missing or disconnected signal, revoked consent, or a changed seat stops
  injection and cancels the current request. Visor releases its held inputs
  through a still-valid sender or closes the sender session. A signal returning
  after a failure requires fresh local approval.
- The automation overlay is separate. A native Wayland surface receives
  pointer coordinates relative to its own surface. A click-through overlay
  needs compositor support for positioning and tracking without intercepting
  the user's input. The system cursor stays visible where the compositor does
  not expose cursor replacement. [Wayland input and surface model](https://wayland.freedesktop.org/docs/book/Protocol.html)

Before enabling the input backend, add CI contracts for simultaneous physical
and emulated input, sender or interruption-signal loss, and permission
revocation on supported GNOME and KDE versions. The existing backend refusal
contract covers the current absence of that capability.
