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

## KDE first

KWin's native libinput backend sees key, button, and motion events before seat
deduplication, motion coalescing, and keyboard accessibility handling.
RemoteDesktop EIS devices enter input redirection separately. A KWin
implementation can observe the local seat before those transformations and
let the physical event continue to its destination. That boundary also
avoids using device names or timing to identify Visor's EIS stream.
[KWin libinput dispatch](https://github.com/KDE/kwin/blob/f1e7d1d4430586d818d9d893f40b157b0f682dbd/src/backends/libinput/connection.cpp#L276-L391)
[KWin keyboard redirection](https://github.com/KDE/kwin/blob/f1e7d1d4430586d818d9d893f40b157b0f682dbd/src/keyboard_input.cpp#L279-L331)
[KWin EIS dispatch](https://github.com/KDE/kwin/blob/f1e7d1d4430586d818d9d893f40b157b0f682dbd/src/plugins/eis/eiscontext.cpp#L259-L296)

The first candidate is an opt-in KWin RemoteDesktop session that ends on
local takeover. With the opt-in bound to the portal-approved seat and sender,
KWin would latch the interruption before forwarding any more input from
that sender, discard its pending events, release its held keys and buttons,
and close the session. Visor would treat the EIS disconnect or portal
session closure as an interruption, cancel the current operation, and
require fresh local approval before sending again. KWin must advertise
this behavior for the session before Visor enables injection. The public
RemoteDesktop portal currently has no option for it.

An [isolated EI/EIS contract](../tests/wayland/eis-pause-contract.c) puts a
pointer frame in the libeis queue before the simulated takeover. The
server-side latch discards the queued motion and the sender receives the
disconnect. When the server called `eis_device_pause()` during active
emulation, libeis 1.2.1 and 1.5.0 sent no pause event. With
[libeis 1.6.0](https://gitlab.freedesktop.org/libinput/libei/-/blob/1.6.0/src/libeis-device.c#L1704-1728)
the sender received the pause event and the queued motion still reached
the server's discard path.
[libeis pause documentation](https://libinput.pages.freedesktop.org/libei/api/group__libeis-device.html)

The contract runs with a C compiler, `pkg-config`, and the libei and libeis
development libraries. It uses a private socket pair and does not open a
display or read input devices.

```sh
cc -std=c11 -Wall -Wextra -Werror tests/wayland/eis-pause-contract.c \
  $(pkg-config --cflags --libs libei-1.0 libeis-1.0) -o /tmp/visor-eis-pause-contract
/tmp/visor-eis-pause-contract
```

Stock KWin still needs the seat hook and portal capability. The contract
covers transport behavior only. For the first compositor qualification,
check that a key consumed by accessibility handling and a button or
motion during concurrent EIS input both end the opted-in session. Check
that physical input still reaches the desktop under its normal rules,
another session is unaffected, and an unadvertised capability keeps
Visor's input disabled.

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

Before enabling an input backend for a compositor, add CI contracts for
simultaneous physical and emulated input, sender or interruption-signal loss,
and permission revocation on the supported versions of that compositor. The
existing backend refusal contract covers the current absence of that
capability.
