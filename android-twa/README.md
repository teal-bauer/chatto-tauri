# Chatto Android via TWA (prototype)

Prototype for packaging the Chatto Android app as a **Trusted Web Activity**
(TWA) with [Bubblewrap](https://github.com/GoogleChromeLabs/bubblewrap) /
[PWABuilder](https://www.pwabuilder.com/), instead of the Tauri WebView wrapper
in `../src-tauri`.

A TWA runs `chat.chatto.run` in the user's installed Chrome engine, not a
bundled WebView. That means the web app's existing service worker and Web Push
work, so background notifications ride Chrome/FCM with no custom native code.
That is the whole point: it deletes the fragile Kotlin notification service
(`../src-tauri/gen/android/app/src/main/java/run/chatto/desktop/NotificationService.kt`)
and the notification bridge in `../src-tauri/src/lib.rs`, which exist only
because service workers and Web Push do not work in the Tauri/Wry WebView
(upstream `tauri#11500`).

Status: prototype. Not wired into CI. Uses a distinct package id
`run.chatto.twa` so it does not collide with the Tauri app's
`run.chatto.desktop`.

## What's here

- `twa-manifest.json` — Bubblewrap config. `enableNotifications: true` turns on
  Android Notification Delegation (Chrome hands its Web Push notifications to the
  app). Colors, icons, and shortcuts mirror `chat.chatto.run/manifest.webmanifest`.
- `assetlinks.json` — the Digital Asset Links file the server must serve. Links
  the app's signing cert to the origin so the TWA runs full-screen without a URL
  bar. The `sha256_cert_fingerprints` placeholder must be filled with the real
  signing cert fingerprint (see below).

The Android Gradle project, signing keystore, and APK are generated, not
committed (see `.gitignore`).

## Build

```sh
npm i -g @bubblewrap/cli          # needs JDK 17+ and the Android SDK

# First run scaffolds the project from twa-manifest.json and creates a keystore:
bubblewrap init --manifest https://chat.chatto.run/manifest.webmanifest
# then copy this twa-manifest.json over the generated one, or answer the prompts
# to match it (package id run.chatto.twa, notifications enabled).

bubblewrap build                  # produces app-release-signed.apk (+ .aab)
bubblewrap fingerprint list       # prints the SHA256 to put in assetlinks.json
```

Or upload `https://chat.chatto.run/manifest.webmanifest` at
[pwabuilder.com](https://www.pwabuilder.com/) and download the Android package,
which wraps the same Bubblewrap flow.

## Server-side requirement

Serve the completed `assetlinks.json` at:

```
https://chat.chatto.run/.well-known/assetlinks.json
```

with the real signing-cert SHA256 (from `bubblewrap fingerprint list` or
`keytool -list -v -keystore ...`). Without a matching asset link the app still
runs, but with a browser URL bar instead of full-screen.

## Notifications

`enableNotifications: true` + `POST_NOTIFICATIONS` (Android 13+, which Chrome
requests) means Chrome receives the site's Web Push over its own FCM channel and
delegates the notification to the app. Chatto already ships the service worker
`push` handler and Declarative Web Push payloads, so nothing app-side is needed.
This replaces the foreground-service WebSocket the Tauri build relies on, and
with it the Doze / OEM-battery / no-FCM fragility.

## Tradeoffs vs the Tauri Android wrapper

| Area | TWA | Tauri WebView |
|------|-----|---------------|
| Background notifications | Chrome/FCM Web Push, no native code | Foreground-service WebSocket, ~600 lines, fragile |
| WebRTC calls | Real Chrome getUserMedia | WebView, permission handling via Wry |
| Edge-to-edge / keyboard / viewport | Handled by Chrome | Custom JS + Kotlin inset shims |
| External links / add-server OAuth | Custom Tabs, shared session | Injected link interception |
| Runtime server-URL override | No, pinned to one origin at build | Yes, settings screen |
| Maintenance | Manifest + asset links | Native code coupled to Chatto's realtime protocol |

The one real loss is runtime server switching: a TWA is pinned to
`chat.chatto.run`. Self-hosted servers are still reachable through Chatto's
in-app federation ("add server"), so this only matters for someone whose primary
server is self-hosted and who never signs in to `chat.chatto.run`.

iOS is out of scope. PWABuilder's iOS output is a WKWebView wrapper with the same
no-service-worker / no-Web-Push limits as Tauri, so it is deferred either way.
