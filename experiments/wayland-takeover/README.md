# KWin physical-input takeover prototype

This experiment implements the compositor and portal work for [Visor #17](https://github.com/hyperme-sh/visor/issues/17). The patches apply to KWin 6.3.6, the KDE RemoteDesktop portal 6.3.5, and xdg-desktop-portal 1.20.3. The checkout revisions are pinned in [qualify.sh](qualify.sh). Both KDE patches use Debian 13's Plasma dependency floor so the experiment can build against the distro SDK.

## Opt-in

The experimental `ConnectToEISWithLocalTakeover` method is available after a successful RemoteDesktop `Start` with fresh local consent and persistence disabled. The public portal validates the caller and session. The KDE backend checks that the request came from the portal, checks for local approval, and asks KWin to create a protected sender. A missing method or inactive native seat fails the request. The caller must request this method explicitly because the standard `ConnectToEIS` method retains its existing behavior.

KWin observes mouse motion, button presses and key presses as it dispatches the native libinput seat, before coalescing motion or applying keyboard accessibility rules. Events sourced from Visor's EIS stream take a separate route. Local input devices routed through libinput can also interrupt a session when they provide accessibility input through a virtual kernel device.

Once KWin observes the local input, it blocks dispatch for every opted-in sender on that seat and releases emulated holds before forwarding the local event. It then destroys the blocked EIS contexts and sends a targeted close notification to the KDE backend, which closes the corresponding RemoteDesktop sessions. The public portal forwards session closure to the client. Input KWin already applied before observing the local event is outside the cutoff. The signal contains a session cookie and carries no keycode, button or pointer data.

## Verification

The [prototype workflow](../../.github/workflows/wayland-takeover-prototype.yml) compiles the patches in Debian 13. Its KWin contract feeds mouse motion, button presses and key presses from the project's mock libinput queue through the production dispatcher. It uses real EI sockets with pending input and checks that the local event still reaches the device, holds are released, the opted-in sender disconnects, and the ordinary sender keeps working. The portal contract covers explicit opt-in, session closure, and rejection when the backend lacks the method.

The contract uses simulated native-device events in an isolated process. Qualification on physical hardware and Visor's production sender and overlay remain separate work. The prototype is not part of Visor's packages and native Wayland injection stays disabled.
