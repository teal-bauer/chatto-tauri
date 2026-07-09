# Chatto Desktop

<p align="center">
  <img src=".github/social-preview.png" alt="Chatto Desktop" width="640">
</p>

Native desktop and Android wrapper for [Chatto](https://chatto.run), built with [Tauri v2](https://v2.tauri.app/).

It loads your Chatto server in a native webview and adds the things a browser tab can't: OS notifications while the app is in the background, a system tray, auto-start, deep links, and an unread badge.

## Features

- **Background notifications.** Watches the Chatto realtime WebSocket and raises native OS notifications while the window is unfocused. On Android a foreground service keeps the connection alive so alerts arrive while the app is backgrounded. Tapping one opens the room.
- **System tray** (desktop): show/hide, settings, autostart toggle, close-to-tray.
- **Auto-start at login** (desktop).
- **Auto-updater** (desktop): checks GitHub Releases and installs in place.
- **Deep links**: `chatto://` opens the matching room.
- **External links** open in the system browser. The add-server OAuth flow stays inside the webview so the redirect chain completes.
- **Unread badge** on the dock/taskbar, driven by the web app's Badging API.
- **Window state** persists across restarts (desktop). Android runs edge-to-edge with keyboard-aware insets.

Point it at a different server from the settings screen (`chatto://` icon in the sidebar on mobile, tray/menu on desktop). The default is `https://chat.chatto.run`.

## Development

### Prerequisites

- [mise](https://mise.jdx.dev/) for the toolchain (Node, Rust, pnpm; Android SDK/NDK for mobile builds)
- macOS, Linux, or Windows with the [Tauri v2 prerequisites](https://v2.tauri.app/start/prerequisites/)

### Setup

```sh
mise install                    # pinned Node, Rust, pnpm
pnpm install                    # frontend dependencies
mise exec -- pnpm tauri dev     # run in dev mode
```

Run every toolchain command through `mise exec --` so the pinned versions are used. `mise trust` the repo first on a fresh checkout.

### Build

```sh
mise exec -- pnpm tauri build                    # .app/.dmg (macOS), .msi (Windows), .deb/.AppImage (Linux)
mise exec -- pnpm tauri android build --apk      # Android APK
```

## Notes

The notification path speaks Chatto's realtime protocol directly: it reads the binary-protobuf `/api/realtime` WebSocket, then hydrates the message body over ConnectRPC (`/api/connect`). This is coupled to the server's current API, so it tracks the [Chatto](https://chatto.run) release it targets.

iOS is not built yet.

## License

[AGPL-3.0-or-later](LICENSE)
