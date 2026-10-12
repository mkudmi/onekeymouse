# OneKeyMouse — product and implementation spec

## Goal
Use one physical keyboard and mouse to control one Windows PC and one Mac on the same local Wi-Fi network. Pointer movement between screens selects the active computer; keyboard and mouse events go only to that computer.

## User setup
The user's desk is arranged as an L: a Windows laptop is on the left, a second Windows display is at upper right, and a Mac display is directly below that Windows display. The Windows laptop and upper display belong to the same Windows computer. The physical keyboard and mouse are attached to the Windows laptop, which is the input source; the Mac is the receiver in the MVP. The screen layout must be configurable because physical monitor geometry and scaling vary.

## MVP scope
- Setup app on macOS and Windows with a small local panel; the current prototype delegates system input capture and networking to an installed Deskflow engine.
- Find peers on the local network using mDNS; no account, cloud relay, or internet dependency.
- Allow a manual IP/port connection if the Wi-Fi access point blocks multicast discovery.
- Encrypted peer connection with Deskflow's fingerprint verification enabled. Discovery metadata alone must never authorize input forwarding.
- Windows owns the physical keyboard and mouse and acts as the input source; Mac receives forwarded input. Evaluate bidirectional control later.
- Configure screen rectangles/edges, including an edge segment that connects the upper-right Windows display to the Mac display below it.
- Transfer pointer movement, buttons, wheel, key up/down, and modifier state. Keyboard focus follows the pointer's active machine.
- Escape hatch: configurable local hotkey to release remote control and return to the source computer.
- Clear connection and macOS permission state; disconnect immediately when peer authentication or network session is lost.

## Out of scope for MVP
Clipboard, file transfer, remote display/video, multiple receiver machines, login-screen/UAC control, gaming/raw input, and automatic OS monitor-layout ingestion.

## Platform constraints
- macOS global event interception/injection requires user-granted system permissions. Show accurate guidance and do not attempt to bypass TCC.
- Windows input capture/injection will use documented user-mode APIs initially; elevated secure desktop is out of scope.
- The network must be treated as untrusted even when it is the home Wi-Fi: pair explicitly, authenticate, encrypt, and validate all messages.

## Architecture direction
- Rust app owns local setup UI, mDNS discovery, layout configuration, and engine lifecycle.
- Deskflow is an external input engine dependency for capture, suppression, injection, and direct LAN transport. The prototype does not bundle it.
- Use mDNS only for discovery. The input data path is direct peer-to-peer and protected by Deskflow TLS with fingerprint checks.
- The local OneKeyMouse control panel binds to loopback only.

## Milestones
1. **Setup scaffold:** local panel, mDNS discovery, and desk-layout view.
2. **Engine launcher (current):** start installed Deskflow core in Windows server or Mac client role; write TLS-enabled settings and a partial-edge configuration.
3. **First-run pairing:** confirm how Deskflow GUI and the Rust launcher share trust prompts and macOS permissions; handle reconnect and released input on engine exit.
4. **Layout and packaging:** detect real Windows monitor bounds, set edge mapping accurately, package the engine dependency, and sign/notarize installers.

## Acceptance criteria
- Both devices can find each other on the same Wi-Fi without a cloud account; manual address entry is available if mDNS is blocked.
- Only a paired peer can exchange control messages; wire traffic is encrypted.
- Cursor crosses only configured edge segments and lands at a predictable normalized position.
- Keyboard, modifiers, mouse buttons, and wheel follow the active target; held keys/buttons are released on disconnect.
- User can quickly return control locally with a hotkey.
- macOS setup explains and detects required permissions.

## Known limitations
- The user must install Deskflow separately on both computers; OneKeyMouse does not currently bundle it.
- The edge percentage is adjustable but initially uses a sample geometry. It is not yet derived from the actual Windows display arrangement.
- Cross-computer behavior and TLS fingerprint prompts still need end-to-end verification on the user's Windows and Mac devices.
- The setup UI now opens in a Tauri desktop window. macOS DMG and Windows NSIS installers are configured in GitHub Actions, but have not yet been produced or signed.
