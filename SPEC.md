# OneKeyMouse — product and implementation spec

## Goal
Use one physical keyboard and mouse to control one Windows PC and one Mac on the same local Wi-Fi network. Pointer movement between screens selects the active computer; keyboard and mouse events go only to that computer.

## User setup
The user's desk is arranged as an L: a Windows laptop is on the left, a second Windows display is at upper right, and a Mac display is directly below that Windows display. The Windows laptop and upper display belong to the same Windows computer. The physical keyboard and mouse are attached to the Windows laptop, which is the input source; the Mac is the receiver in the MVP. The screen layout must be configurable because physical monitor geometry and scaling vary.

## MVP scope
- Setup app on macOS and Windows with a small local panel and a native OneKeyMouse input engine. No separate input-sharing application is installed.
- Find peers on the local network using mDNS; no account, cloud relay, or internet dependency.
- Allow a manual IP/port connection if the Wi-Fi access point blocks multicast discovery.
- Encrypted peer connection with explicit pairing. Discovery metadata alone must never authorize input forwarding.
- Windows owns the physical keyboard and mouse and acts as the input source; Mac receives forwarded input. Evaluate bidirectional control later.
- Configure screen rectangles/edges, including an edge segment that connects the upper-right Windows display to the Mac display below it.
- Transfer pointer movement, buttons, wheel, key up/down, and modifier state. Keyboard focus follows the pointer's active machine.
- Escape hatch: configurable local hotkey to release remote control and return to the source computer.
- Clear connection and macOS permission state; disconnect immediately when peer authentication or network session is lost.

## Out of scope for MVP
Clipboard, file transfer, remote display/video, multiple receiver machines, login-screen/UAC control, gaming/raw input, and remote input across multiple Mac monitors (the current target is its main display).

## Platform constraints
- macOS global event interception/injection requires user-granted system permissions. Show accurate guidance and do not attempt to bypass TCC.
- Windows input capture/injection will use documented user-mode APIs initially; elevated secure desktop is out of scope.
- The network must be treated as untrusted even when it is the home Wi-Fi: pair explicitly, authenticate, encrypt, and validate all messages.

## Architecture direction
- Rust app owns the local setup UI, mDNS discovery, layout configuration, pairing, transport, and platform input engines.
- Windows captures physical input through native OS hooks after an explicit Mac pairing is approved, then sends authenticated event packets over an encrypted peer connection.
- macOS injects authorized input with Core Graphics and listens for the peer connection. macOS capture permissions are explained clearly and may be needed for bidirectional use.
- Use mDNS only for discovery. Discovery is unauthenticated; pairing must authenticate the peer and establish session keys before input can flow.
- The local OneKeyMouse control panel binds to loopback only.

## Milestones
1. **Setup scaffold:** local panel, mDNS discovery, and desk-layout view.
2. **Native input engine (in progress):** pairing, encrypted transport, Windows capture, and macOS injection are implemented in the prototype; edge-triggered pointer handoff is implemented; persistent trust and device end-to-end verification remain.
3. **Reconnect and fail-safe:** reestablish paired sessions safely, release held inputs on disconnect, and keep a local escape hotkey.
4. **Layout and distribution:** live detection of Windows/Mac display bounds and edge mapping are implemented; signing/notarization remains.

## Acceptance criteria
- Both devices can find each other on the same Wi-Fi without a cloud account; manual address entry is available if mDNS is blocked.
- Only a paired peer can exchange control messages; wire traffic is encrypted.
- Cursor crosses only configured edge segments and lands at a predictable normalized position.
- Keyboard, modifiers, mouse buttons, and wheel follow the active target; held keys/buttons are released on disconnect.
- User can quickly return control locally with a hotkey.
- macOS setup explains and detects required permissions.

## Known limitations
- Approved sessions remain local until a downward crossing of the configured exposed Windows bottom edge. Upward crossing of the Mac main display returns the cursor to Windows; Ctrl+Alt+F12 is the emergency return shortcut. Entry is deferred while keys or mouse buttons are held.
- Pairing is approved in the Windows UI for each new session; saved peer identity, reconnect behavior, and timeout/release verification remain.
- Windows capture reads actual monitor bounds with per-monitor DPI awareness. The percentage selects the start of the edge segment across the desktop width; internal monitor seams are excluded. The UI shows detected local and peer monitor geometry, main display and dimensions. Connected peers exchange inventories every two seconds, and the capture geometry updates automatically after monitor changes. Settings currently last for the app run. The Mac receiver uses its main display.
- Cross-computer behavior still needs end-to-end verification on the user's Windows and Mac devices. The custom encrypted protocol currently uses explicit per-session approval rather than stored pairing identity.
- The setup UI opens in a Tauri desktop window. The installer workflows build the native prototype; signing, persistent pairing, and cross-device verification remain.

## Cursor handoff implementation
- Protocol version 2 adds normalized pointer entry and return-control messages inside the encrypted session. Update both devices together.
- The Windows motion hook decides entry synchronously before local dispatch. While remote, it suppresses pointer motion and parks the source cursor away from desktop edges to preserve movement in every direction. Keys/buttons/wheel are routed only while remote.
- The receiver maintains an absolute pointer position, clamps it to the main Mac display, and returns control when upward motion crosses its top. Pointer movement uses drag events when a button is held.
- A persistent duplex reader prevents partially received frames being lost when capture or heartbeat wins a select. Both peers send heartbeats every 500 ms, time out an incomplete/absent frame after 3 seconds, and bound writes to 1 second. Remote held inputs are released on return/disconnect; pending physical releases remain suppressed locally after return.
- macOS checks event-posting permission before connecting and during the session; permission removal closes the session and returns Windows input.

## Automatic display inventory
- Windows enumerates active monitors and device names through EnumDisplayMonitors/GetMonitorInfoW, using physical pixel coordinates with per-monitor DPI awareness.
- Mac enumerates all active Core Graphics displays, including the main display. The inventory reports both Quartz coordinate bounds in points and display pixel dimensions, preserving Retina scaling differences.
- The setup UI displays each computer’s OS monitor arrangement. The physical position of one computer relative to the other cannot be inferred by the OS; the current Windows-above-Mac connection and adjustable edge segment remain.
- Inventory exchange is encrypted and bounded to 32 validated displays. Layout changes update within two seconds while connected; the panel refreshes every five seconds. Disconnect clears peer inventory. A change to Windows geometry or the main Mac target geometry returns input locally before further control.
