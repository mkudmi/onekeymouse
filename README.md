# OneKeyMouse

OneKeyMouse is a desktop setup app for sharing one keyboard and mouse between Windows and macOS over a local network. It discovers peers over mDNS and uses a native Rust input engine with a dedicated encrypted peer protocol. No separate input-sharing app is installed.

## Installers

The Tauri desktop shell produces a macOS `.dmg` and a Windows NSIS `.exe` installer. GitHub Actions builds these on native macOS and Windows runners. Run the `Build installers` workflow manually from the Actions tab, then download the `onekeymouse-macos-dmg` and `onekeymouse-windows-installer` artifacts from that run. Pushing a `v*` tag also builds both artifacts.

A local macOS build can be produced with:

```sh
npm exec --yes --package @tauri-apps/cli@2.12.2 -- tauri build --bundles dmg
```

The first packages are unsigned. macOS Gatekeeper and Windows SmartScreen may show publisher warnings until the app is signed with developer certificates. Signing and Apple notarization require the owner's certificates and credentials.

## Native input engine

OneKeyMouse has its own pairing protocol, encrypted event transport, and app lifecycle; it uses native OS input facilities through Rust libraries packaged inside the app. A Windows user must approve each new Mac pairing in the Windows app. macOS requires explicit user-granted Accessibility approval before remote input can be injected. No separate input-sharing app needs to be installed.

The app opens as a native desktop window. Start OneKeyMouse on Windows, then connect from the Mac using the Windows computer's private IP address. Both computers must be on the same local network. After pairing, input stays on Windows until the pointer reaches the configured segment of an exposed bottom monitor edge. It enters the top of the Mac's main display at the corresponding horizontal position. Move upward through the Mac's top edge to return to Windows, or press **Ctrl+Alt+F12** on the Windows keyboard. Stop and network loss also return local control and release remote keys/buttons.

The Windows slider selects the start of the handoff segment as a percentage of the entire Windows desktop width. All active displays on Windows and Mac are detected automatically, including their coordinates, primary display and pixel dimensions. The interface shows each computer’s actual monitor arrangement. Connected peers exchange updated inventories every two seconds; internal Windows monitor seams do not transfer control. Release keys and mouse buttons before entering the Mac. Connecting/disconnecting a display or changing its bounds updates the layout automatically. A geometry change affecting remote control returns input to Windows safely. The Mac receiver currently targets its main display. Slider settings are kept for the current app run.

Both computers must run this updated build (protocol version 2). Older builds cannot pair with it.

See [SPEC.md](SPEC.md) for behavior and remaining limitations.

## Verification

Run `cargo test` and `cargo check` on the native platform. For a device test, connect and approve the Mac, verify Windows input remains local, cross the configured lower edge, test typing/clicking/scrolling and dragging on Mac, return upward, then test Ctrl+Alt+F12 while modifiers are held. Finally disconnect Wi-Fi while holding a remote key/button: input should return to Windows within a few seconds and remote held inputs should be released. Repeat across both Windows displays and outside the configured segment.

## Windows download verification

Windows builds scan the finished installer and its extracted payload with Microsoft Defender before uploading the installer artifact. The separate `onekeymouse-windows-security-report` artifact records the scan, definition version, Authenticode status and executable hashes. `SHA256SUMS.txt` accompanies the installer. An unavailable scanner or a detected threat fails the publishing step.

The installer displays Microsoft's WebView2 setup if the runtime is missing; it does not install that dependency silently. Current builds are unsigned. A passing Defender scan does not guarantee that Chrome Safe Browsing or SmartScreen will accept a download. A Chrome `Dangerous download blocked` result requires investigation or review by Google; do not treat changing the filename, archive or browser as a security fix.

The manually triggered `Inspect Windows download` workflow can scan an exact historical Actions artifact by build run id without executing the installer. Trusted Windows signing still requires an owner's code-signing certificate or signing service.
