# OneKeyMouse

OneKeyMouse is a desktop setup app for sharing one keyboard and mouse between Windows and macOS over a local network. It discovers peers over mDNS, configures the screen edge and starts Deskflow with TLS and peer fingerprint checks enabled.

## Installers

The Tauri desktop shell produces a macOS `.dmg` and a Windows NSIS `.exe` installer. GitHub Actions builds these on native macOS and Windows runners. Run the `Build installers` workflow manually from the Actions tab, then download the `onekeymouse-macos-dmg` and `onekeymouse-windows-installer` artifacts from that run. Pushing a `v*` tag also builds both artifacts.

The Windows installer cannot be built on this Mac; it is built by the Windows CI runner. The Mac `.dmg` can be built locally with:

```sh
cargo install tauri-cli --version "^2" --locked
cargo tauri build --bundles dmg
```

The first packages are unsigned. macOS Gatekeeper and Windows SmartScreen may show publisher warnings until the app is signed with developer certificates. Signing and Apple notarization require the owner's certificates and credentials.

## Deskflow dependency

Install Deskflow on both computers first. On macOS, use the official Homebrew commands `brew tap deskflow/tap` and `brew install deskflow`. On Windows, download the Deskflow installer from the [official releases](https://github.com/deskflow/deskflow/releases). OneKeyMouse starts the separately installed Deskflow core and opens its GUI for system permissions and trusted-peer confirmation. On macOS, grant the requested Accessibility permission.

The app opens as a native desktop window. On Windows, start the server. On the Mac, enter the Windows computer's local IP address (or choose the discovered computer) and start the client. Keep both computers on the same Wi-Fi network. The Windows config connects a lower edge segment to the Mac's upper edge. Adjust the segment start to match where the upper Windows monitor begins across the combined Windows desktop; the default 42.9% is based on the sample 900-pixel laptop plus 1200-pixel monitor widths.

See [SPEC.md](SPEC.md) for behavior and remaining limitations.
