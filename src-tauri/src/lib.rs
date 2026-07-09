use tauri::Manager;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_store::StoreExt;

#[cfg(desktop)]
use tauri::{
    menu::{AboutMetadataBuilder, CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri::{WebviewUrl, WebviewWindowBuilder};

use serde_json::json;
use std::sync::Mutex;
#[cfg(desktop)]
use std::sync::atomic::{AtomicI32, Ordering};

// The configured server origin host (e.g. "chat.chatto.run"), captured at window
// creation. External-link handling keys off this: while the webview sits on a
// FOREIGN host (an OIDC provider mid-flow) links stay inside the webview; while
// on the configured origin host, foreign-host clicks are externalized.
static CONFIGURED_ORIGIN_HOST: Mutex<Option<String>> = Mutex::new(None);

const DEFAULT_SERVER_URL: &str = "https://chat.chatto.run";

const NOTIFICATION_BRIDGE_JS: &str = r#"
(function() {
    if (window.__chattoNotificationBridged) return;
    window.__chattoNotificationBridged = true;

    // Override the Page Visibility API so the web app reports the correct
    // presence state. window.__chattoWindowHidden is set by Rust via
    // window.eval() on every WindowEvent::Focused change.
    try {
        Object.defineProperty(document, 'visibilityState', {
            get: function() { return window.__chattoWindowHidden ? 'hidden' : 'visible'; },
            configurable: true,
        });
        Object.defineProperty(document, 'hidden', {
            get: function() { return !!window.__chattoWindowHidden; },
            configurable: true,
        });
    } catch(e) {}

    // Forward Badging API calls to the native dock/taskbar badge. The web app
    // uses navigator.setAppBadge(count) / clearAppBadge() for the unread count.
    try {
        navigator.setAppBadge = function(count) {
            if (window.__TAURI_INTERNALS__) {
                var n = (typeof count === 'number' && isFinite(count)) ? Math.floor(count) : null;
                window.__TAURI_INTERNALS__.invoke('set_badge', { count: n }).catch(function() {});
            }
            return Promise.resolve();
        };
        navigator.clearAppBadge = function() {
            if (window.__TAURI_INTERNALS__) {
                window.__TAURI_INTERNALS__.invoke('set_badge', { count: null }).catch(function() {});
            }
            return Promise.resolve();
        };
    } catch(e) {}

    // Deduplication: track recently fired notifications by event_id (fallback
    // notification_id) to avoid firing twice when multiple sockets deliver the
    // same event. Keying on the event id, not roomId, so distinct rapid
    // messages in the same room are not wrongly dropped.
    var __chattoRecentNotifKeys = {};
    function __chattoShouldFire(key) {
        var now = Date.now();
        for (var k in __chattoRecentNotifKeys) {
            if (now - __chattoRecentNotifKeys[k] > 3000) delete __chattoRecentNotifKeys[k];
        }
        if (__chattoRecentNotifKeys[key] && now - __chattoRecentNotifKeys[key] < 3000) return false;
        __chattoRecentNotifKeys[key] = now;
        return true;
    }

    // Minimal hand-rolled protobuf field walker
    // The realtime socket speaks binary protobuf. We only need to reach a few
    // fields, so walk the wire format directly instead of pulling in a codec.
    // wire types: 0=VARINT, 1=I64, 2=LEN, 5=I32. Field/length varints are small
    // enough to stay within 32 bits here.
    function __pbDecodeUtf8(buf, s, e) {
        try { return new TextDecoder('utf-8').decode(buf.subarray(s, e)); }
        catch(_) {
            var out = '';
            for (var i = s; i < e; i++) out += String.fromCharCode(buf[i]);
            try { return decodeURIComponent(escape(out)); } catch(__) { return out; }
        }
    }
    // Returns the LAST field matching `want` in buf[start,end) as
    // {wire, value, start, end} (start/end are byte offsets for LEN fields), or null.
    function __pbField(buf, start, end, want) {
        var i = start, res = null;
        while (i < end) {
            var tag = 0, shift = 0, b;
            do { b = buf[i++]; tag |= (b & 0x7f) << shift; shift += 7; } while (b & 0x80 && i < end);
            var field = tag >>> 3, wire = tag & 7;
            if (wire === 0) {
                var v = 0, m = 1, c;
                do { c = buf[i++]; v += (c & 0x7f) * m; m *= 128; } while (c & 0x80 && i < end);
                if (field === want) res = { wire: 0, value: v, start: 0, end: 0 };
            } else if (wire === 2) {
                var len = 0, s2 = 0, d;
                do { d = buf[i++]; len |= (d & 0x7f) << s2; s2 += 7; } while (d & 0x80 && i < end);
                if (field === want) res = { wire: 2, value: 0, start: i, end: i + len };
                i += len;
            } else if (wire === 1) { i += 8; }
            else if (wire === 5) { i += 4; }
            else { break; }
        }
        return res;
    }
    function __pbStr(buf, f) { return (f && f.wire === 2) ? __pbDecodeUtf8(buf, f.start, f.end) : null; }

    // Decode a RealtimeServerFrame -> RealtimeEventEnvelope -> notification_created.
    function __chattoHandleFrame(buf) {
        // On Android the native NotificationService owns the background path;
        // firing here too would double up.
        if (window.ChattoAndroid) return;
        // RealtimeServerFrame.event = field 3 (LEN).
        var evt = __pbField(buf, 0, buf.length, 3);
        if (!evt || evt.wire !== 2) return;
        // RealtimeEventEnvelope.notification_created = field 60 (LEN). Key on this only.
        var notif = __pbField(buf, evt.start, evt.end, 60);
        if (!notif || notif.wire !== 2) return;
        // RealtimeNotificationCreatedEvent fields.
        var notifId = __pbStr(buf, __pbField(buf, notif.start, notif.end, 1));
        var roomId  = __pbStr(buf, __pbField(buf, notif.start, notif.end, 2));
        var eventId = __pbStr(buf, __pbField(buf, notif.start, notif.end, 3));
        var silentF = __pbField(buf, notif.start, notif.end, 5);
        if (silentF && silentF.wire === 0 && silentF.value !== 0) return; // silent
        if (!roomId) return;
        if (!window.__chattoWindowHidden) return;
        var key = eventId || notifId || roomId;
        if (!__chattoShouldFire(key)) return;
        __chattoFetchEventAndNotify(roomId, eventId);
    }

    // Hydrate title + body via same-origin ConnectRPC (cookies flow
    // automatically) and show a native notification. Prefers the exact event
    // when eventId is known; falls back to the room's latest message otherwise.
    function __chattoFetchEventAndNotify(roomId, eventId) {
        if (!window.__TAURI_INTERNALS__) return;
        var url, body;
        if (eventId) {
            url = '/api/connect/chatto.api.v1.RoomService/GetRoomEventsAround';
            body = { roomId: roomId, eventId: eventId, limit: 1 };
        } else {
            url = '/api/connect/chatto.api.v1.RoomService/GetRoomEvents';
            body = { roomId: roomId, limit: 1 };
        }
        fetch(url, {
            method: 'POST',
            credentials: 'include',
            headers: {'Content-Type': 'application/json'},
            body: JSON.stringify(body)
        })
        .then(function(r) { return r.json(); })
        .then(function(data) {
            var page = data && data.page;
            var events = page && page.events;
            if (!events || !events.length) return;
            var anchor = null;
            if (eventId) {
                for (var i = 0; i < events.length; i++) {
                    if (events[i] && events[i].id === eventId) { anchor = events[i]; break; }
                }
                if (!anchor && typeof page.targetIndex === 'number'
                    && page.targetIndex >= 0 && page.targetIndex < events.length) {
                    var t = events[page.targetIndex];
                    if (t && t.messagePosted) anchor = t;
                }
            }
            if (!anchor) {
                for (var j = events.length - 1; j >= 0; j--) {
                    if (events[j] && events[j].messagePosted) { anchor = events[j]; break; }
                }
            }
            var msg = anchor && anchor.messagePosted && anchor.messagePosted.message;
            var text = msg && msg.body;
            if (!text) return; // not a chat message (join/leave/etc.), suppress
            var actorId = (msg && msg.actorId) || (anchor && anchor.actorId);
            var title = 'Chatto';
            var users = page.includes && page.includes.users;
            if (users && actorId && users[actorId] && users[actorId].displayName) {
                title = users[actorId].displayName;
            }
            window.__TAURI_INTERNALS__.invoke('show_notification', {
                title: title,
                body: text
            }).catch(function() {});
        })
        .catch(function() {});
    }

    // Passively intercept the app's own realtime WebSocket. Chatto uses
    // wss://<host>/api/realtime with binary protobuf frames (binaryType is
    // 'arraybuffer'). We only read messages; the app owns the socket.
    (function() {
        var _WS = window.WebSocket;
        function PatchedWebSocket(url, protocols) {
            var ws = protocols !== undefined ? new _WS(url, protocols) : new _WS(url);
            if (typeof url === 'string' && url.indexOf('/api/realtime') !== -1) {
                ws.addEventListener('message', function(ev) {
                    try {
                        var data = ev.data;
                        if (data instanceof ArrayBuffer) {
                            __chattoHandleFrame(new Uint8Array(data));
                        } else if (typeof Blob !== 'undefined' && data instanceof Blob) {
                            data.arrayBuffer().then(function(ab) {
                                try { __chattoHandleFrame(new Uint8Array(ab)); } catch(_) {}
                            });
                        }
                    } catch(_) {}
                });
            }
            return ws;
        }
        PatchedWebSocket.prototype = _WS.prototype;
        PatchedWebSocket.CONNECTING = _WS.CONNECTING;
        PatchedWebSocket.OPEN = _WS.OPEN;
        PatchedWebSocket.CLOSING = _WS.CLOSING;
        PatchedWebSocket.CLOSED = _WS.CLOSED;
        window.WebSocket = PatchedWebSocket;
    })();

    // Keep Notification API mock for compatibility, reported as granted so
    // the web app does not prompt the user for permission.
    window.Notification = function(title, options) {
        if (window.__TAURI_INTERNALS__) {
            window.__TAURI_INTERNALS__.invoke('show_notification', {
                title: title,
                body: (options && options.body) || ''
            }).catch(function() {});
        }
        this.title = title;
        this.body = (options && options.body) || '';
        this.icon = (options && options.icon) || '';
        this.tag = (options && options.tag) || '';
        this.onclick = null;
        this.onclose = null;
        this.onerror = null;
        this.onshow = null;
        this.close = function() {};
    };
    window.Notification.permission = 'granted';
    window.Notification.requestPermission = function() {
        return Promise.resolve('granted');
    };
})();
"#;

// Ensure the remote page has viewport-fit=cover so env(safe-area-inset-*)
// resolves to real values. On true edge-to-edge mode (Android 15+ default),
// the WebView extends behind status and gesture bars; pages without
// viewport-fit=cover see env() resolve to 0 and their content gets clipped.
#[cfg(mobile)]
const MOBILE_VIEWPORT_FIT_JS: &str = r#"
(function() {
    if (window.__chattoViewportFit) return;
    window.__chattoViewportFit = true;

    function ensureViewport() {
        var meta = document.querySelector('meta[name="viewport"]');
        if (!meta) {
            meta = document.createElement('meta');
            meta.setAttribute('name', 'viewport');
            meta.setAttribute('content', 'width=device-width, initial-scale=1, viewport-fit=cover');
            (document.head || document.documentElement).appendChild(meta);
            return;
        }
        var content = meta.getAttribute('content') || '';
        if (!/viewport-fit\s*=/i.test(content)) {
            meta.setAttribute('content', content.replace(/\s*$/, '') + (content ? ', ' : '') + 'viewport-fit=cover');
        }
    }

    if (document.head) {
        ensureViewport();
    } else {
        var obs = new MutationObserver(function() {
            if (document.head) {
                ensureViewport();
                obs.disconnect();
            }
        });
        obs.observe(document.documentElement, { childList: true, subtree: true });
    }
})();
"#;

// VisualViewport-based keyboard shim. The remote Chatto web app sometimes
// double-pads when the keyboard appears (the WebView shrinks via adjustResize
// AND the page reserves keyboard space itself, leaving whitespace above the
// IME). This shim:
//   - Exposes the visible viewport height as --chatto-vv-height
//   - Exposes the keyboard height (innerHeight - vv.height) as --chatto-vv-kbd
//   - Forces html { height: 100dvh } so layout follows the visible area when
//     the page wasn't designed for it
//   - Scrolls focused inputs into view above the keyboard
#[cfg(target_os = "android")]
const KEYBOARD_VIEWPORT_SHIM_JS: &str = r#"
(function() {
    if (window.__chattoKbdShim) return;
    window.__chattoKbdShim = true;

    function injectStyle() {
        if (document.getElementById('chatto-kbd-shim-style')) return;
        var s = document.createElement('style');
        s.id = 'chatto-kbd-shim-style';
        s.textContent = [
            'html { min-height: 100dvh; }',
            // Pages that use 100vh on body get the dynamic equivalent so they
            // match the visible viewport when the keyboard is up.
            'body { min-height: 100dvh; }'
        ].join('\n');
        (document.head || document.documentElement).appendChild(s);
    }

    function update() {
        var vv = window.visualViewport;
        if (!vv) return;
        var kbd = Math.max(0, window.innerHeight - vv.height);
        var r = document.documentElement;
        if (r && r.style) {
            r.style.setProperty('--chatto-vv-height', vv.height + 'px');
            r.style.setProperty('--chatto-vv-kbd', kbd + 'px');
        }
    }

    function onFocus(e) {
        var t = e.target;
        if (!t || !t.matches) return;
        if (!t.matches('input, textarea, select, [contenteditable=""], [contenteditable="true"]')) return;
        // Wait for the IME to settle before scrolling. 350ms covers most
        // Android IMEs without feeling laggy.
        setTimeout(function() {
            try {
                t.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'smooth' });
            } catch (_) {
                try { t.scrollIntoView(false); } catch(_) {}
            }
        }, 350);
    }

    function init() {
        injectStyle();
        if (window.visualViewport) {
            window.visualViewport.addEventListener('resize', update);
            window.visualViewport.addEventListener('scroll', update);
        }
        document.addEventListener('focusin', onFocus, true);
        update();
    }

    if (document.head) {
        init();
    } else {
        var obs = new MutationObserver(function() {
            if (document.head) {
                init();
                obs.disconnect();
            }
        });
        obs.observe(document.documentElement, { childList: true, subtree: true });
    }
})();
"#;

// Keeps the emoji-reaction bottom sheet (a native <dialog> opened via
// showModal()) open when Android's soft keyboard fires a spurious dialog
// close. Tapping the emoji search input inside the sheet raises the IME,
// and the WebView delivers a 'cancel' (which the web app already guards)
// followed sometimes by a non-cancelable 'close' the app does NOT guard,
// so the whole sheet gets dismissed. We reproduce the same guard generically
// at the document level, for any <dialog>, without coupling to Chatto's CSS.
//
// The key discriminator: the app only ever calls
// HTMLDialogElement.prototype.close() to dismiss a dialog on purpose (emoji
// picked, backdrop tapped, Escape, etc). The Android-engine-initiated close
// that rides along with the IME does not go through that method. So we
// monkeypatch close() to leave a breadcrumb, and treat any 'close' event
// that arrives without that breadcrumb, during the keyboard focus race
// window, as spurious and reopen the dialog.
#[cfg(target_os = "android")]
const DIALOG_KEYBOARD_GUARD_JS: &str = r#"
(function() {
    if (window.__chattoDialogKbdGuard) return;
    window.__chattoDialogKbdGuard = true;

    var proto = window.HTMLDialogElement && window.HTMLDialogElement.prototype;
    if (!proto) return;

    // Mark intentional closes so the 'close' handler below can tell them
    // apart from the engine-initiated ones the Android IME triggers.
    var origClose = proto.close;
    proto.close = function() {
        this.__chattoIntentionalClose = true;
        return origClose.apply(this, arguments);
    };

    // How long after focusing an input inside an open dialog we still treat
    // a close event as keyboard-related. Covers the gap between the IME
    // animating in and the WebView firing its spurious cancel/close pair.
    var KEYBOARD_RACE_MS = 1000;
    var lastDialogInput = null;
    var lastDialogInputAt = 0;

    function raceActive() {
        return (Date.now() - lastDialogInputAt) < KEYBOARD_RACE_MS;
    }

    function isTextInput(el) {
        if (!el) return false;
        if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA') return true;
        return !!(el.isContentEditable);
    }

    // Track the focused input any time focus lands inside an open dialog,
    // so we know both when the keyboard race window is active and which
    // element to restore focus to if we have to reopen.
    document.addEventListener('focusin', function(e) {
        var t = e.target;
        if (!isTextInput(t)) return;
        var dialog = t.closest && t.closest('dialog');
        if (!dialog || !dialog.open) return;
        lastDialogInput = t;
        lastDialogInputAt = Date.now();
    }, true);

    // Swallow the first spurious event of the pair: a 'cancel' fired by the
    // WebView while the keyboard race is active (or while a dialog input is
    // still focused). Harmless if the app already prevents it too.
    document.addEventListener('cancel', function(e) {
        var d = e.target;
        if (!(d instanceof HTMLDialogElement)) return;
        var active = document.activeElement;
        var activeInsideDialog = isTextInput(active) && d.contains(active);
        if (raceActive() || activeInsideDialog) {
            e.preventDefault();
        }
    }, true);

    // Catch the second, non-preventable event of the pair: a 'close' that
    // actually dismissed the dialog. Reopen it if it wasn't an intentional
    // app-initiated close and we're still inside the keyboard race window.
    document.addEventListener('close', function(e) {
        var d = e.target;
        if (!(d instanceof HTMLDialogElement)) return;

        if (d.__chattoIntentionalClose) {
            // The app closed this on purpose (emoji picked, backdrop tap,
            // Escape...). Clear the flag and leave it closed.
            d.__chattoIntentionalClose = false;
            return;
        }

        if (!raceActive()) {
            // Not keyboard-related; this is a safety valve so we never trap
            // a dialog the user is legitimately trying to dismiss.
            return;
        }

        requestAnimationFrame(function() {
            if (d.open) return;
            try {
                d.showModal();
            } catch (_) {
                return;
            }
            // showModal() moves focus to the dialog itself; restore focus to
            // the input the user was typing in so the soft keyboard the tap
            // summoned stays up instead of dismissing on the refocus.
            if (lastDialogInput && d.contains(lastDialogInput)) {
                try { lastDialogInput.focus(); } catch (_) {}
            }
        });
    }, true);
})();
"#;

#[cfg(target_os = "android")]
const ACTIVE_ROOM_TRACKER_JS: &str = r#"
(function() {
    if (window.__chattoRoomTracker) return;
    window.__chattoRoomTracker = true;

    function reportRoom() {
        // Tolerate both /chat/<roomId> (current) and /chat/<spaceId>/<roomId>
        // (legacy, in case it lingers somewhere). roomId is the segment that
        // ends the path or precedes the next slash.
        var m = window.location.pathname.match(/^\/chat\/(?:[^\/]+\/)?([^\/?#]+)/);
        var roomId = m ? m[1] : '';
        // Call Android JavascriptInterface directly, no Rust IPC needed
        if (window.ChattoAndroid && window.ChattoAndroid.setActiveRoom) {
            window.ChattoAndroid.setActiveRoom(roomId);
        }
    }

    var _pushState = history.pushState;
    history.pushState = function() {
        _pushState.apply(history, arguments);
        reportRoom();
    };
    var _replaceState = history.replaceState;
    history.replaceState = function() {
        _replaceState.apply(history, arguments);
        reportRoom();
    };
    window.addEventListener('popstate', reportRoom);

    reportRoom();
})();
"#;

#[cfg(mobile)]
const MOBILE_SETTINGS_BUTTON_JS: &str = r#"
(function() {
    if (window.__chattoSettingsInjected) return;
    window.__chattoSettingsInjected = true;

    function injectSidebarItem() {
        if (document.getElementById('chatto-app-settings')) return;
        var nav = document.querySelector('nav.flex.flex-col');
        if (!nav) return;
        var hasSettingsLinks = nav.querySelector('a[href*="/settings"]');
        if (!hasSettingsLinks) return;

        var a = document.createElement('a');
        a.id = 'chatto-app-settings';
        a.href = '#';
        a.className = 'sidebar-item';
        a.innerHTML = '<span class="sidebar-icon iconify uil--wrench"></span> App Settings';
        a.addEventListener('click', function(e) {
            e.preventDefault();
            if (window.__TAURI_INTERNALS__) {
                window.__TAURI_INTERNALS__.invoke('open_settings').catch(function() {});
            }
        });
        nav.appendChild(a);
    }

    var observer = new MutationObserver(function() { injectSidebarItem(); });
    observer.observe(document.documentElement, { childList: true, subtree: true });
    if (document.body) injectSidebarItem();
})();
"#;

const EXTERNAL_LINK_JS: &str = r#"
(function() {
    if (window.__chattoExternalLinks) return;
    window.__chattoExternalLinks = true;

    var serverHost = window.location.hostname;
    // Updated by check_instance_flow on page load. While true, the page is on a
    // FOREIGN host (an OIDC provider mid-flow), keep every link inside the
    // webview so the redirect chain can complete and land back on the origin.
    var inInstanceFlow = false;

    function isExternal(url) {
        try {
            var u = new URL(url, window.location.href);
            if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
            return u.hostname !== serverHost
                && u.hostname !== 'localhost'
                && u.hostname !== 'tauri.localhost';
        } catch(e) { return false; }
    }

    function shouldExternalize(url) {
        if (inInstanceFlow) return false;
        return isExternal(url);
    }

    function openExternal(url) {
        if (window.__TAURI_INTERNALS__) {
            window.__TAURI_INTERNALS__.invoke('plugin:opener|open_url', { url: url })
                .catch(function() { window.__TAURI_INTERNALS__.invoke('open_external_url', { url: url }).catch(function() {}); });
        }
    }

    function installHandlers() {
        document.addEventListener('click', function(e) {
            var a = e.target.closest('a[href]');
            if (!a) return;
            var href = a.href;
            if (shouldExternalize(href)) {
                e.preventDefault();
                e.stopPropagation();
                openExternal(href);
            }
        }, true);

        var origOpen = window.open;
        window.open = function(url) {
            if (url && shouldExternalize(url)) {
                openExternal(url);
                return null;
            }
            return origOpen.apply(window, arguments);
        };
    }

    if (window.__TAURI_INTERNALS__) {
        window.__TAURI_INTERNALS__.invoke('check_instance_flow', { url: window.location.href })
            .then(function(active) { inInstanceFlow = !!active; })
            .catch(function() {})
            .finally(installHandlers);
    } else {
        installHandlers();
    }
})();
"#;

// Template icon for macOS menu bar (black on transparent, used as template image)
#[cfg(desktop)]
const TRAY_ICON_BYTES: &[u8] = include_bytes!("../icons/tray-icon.png");

// Zoom level stored as percentage (100 = 100%). Step is 10%.
#[cfg(desktop)]
static ZOOM_LEVEL: AtomicI32 = AtomicI32::new(100);

#[cfg(desktop)]
fn apply_zoom(window: &tauri::WebviewWindow, delta: i32) {
    let level = if delta == 0 {
        ZOOM_LEVEL.store(100, Ordering::SeqCst);
        100
    } else {
        // fetch_update uses CAS internally, safe under concurrent access
        let prev = ZOOM_LEVEL
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some((current + delta).clamp(30, 300))
            })
            .unwrap(); // always succeeds since closure always returns Some
        (prev + delta).clamp(30, 300)
    };
    let _ = window.set_zoom(level as f64 / 100.0);
    if let Ok(store) = window.app_handle().store("config.json") {
        store.set("zoom_level", json!(level));
        let _ = store.save();
    }
}

#[tauri::command]
fn set_server_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let parsed: tauri::Url = url.parse().map_err(|e| format!("Invalid URL: {e}"))?;

    // Skip reachability check for localhost (may use self-signed certs)
    let is_localhost = parsed
        .host_str()
        .map(|h| h == "localhost" || h == "127.0.0.1" || h == "::1")
        .unwrap_or(false);

    if !is_localhost {
        match ureq::head(parsed.as_str())
            .timeout(std::time::Duration::from_secs(10))
            .call()
        {
            Ok(_) => {}
            Err(ureq::Error::Status(_, _)) => {
                // Any HTTP response means the server is reachable
            }
            Err(ureq::Error::Transport(e)) => {
                let reason = match e.kind() {
                    ureq::ErrorKind::Dns => "Server not found, check the address",
                    ureq::ErrorKind::ConnectionFailed => "Could not connect to server",
                    ureq::ErrorKind::Io => "Connection error",
                    _ => "Server unreachable",
                };
                return Err(format!("{reason} ({e})"));
            }
        }
    }

    let store = app.store("config.json").map_err(|e| e.to_string())?;
    store.set("server_url", json!(url));
    store.save().map_err(|e| e.to_string())?;

    let window = app.get_webview_window("main").ok_or("no main window")?;
    window.navigate(parsed).map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_server_url(app: tauri::AppHandle) -> Result<(), String> {
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    store.delete("server_url");
    store.save().map_err(|e| e.to_string())?;

    let window = app.get_webview_window("main").ok_or("no main window")?;
    let default_url: tauri::Url = DEFAULT_SERVER_URL.parse().expect("invalid DEFAULT_SERVER_URL");
    window.navigate(default_url).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_server_url(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    Ok(store
        .get("server_url")
        .and_then(|v| v.as_str().map(|s| s.to_string())))
}

// Called by the injected JS on every page load with the current URL. Returns
// whether outbound links should stay inside the webview. Rule: the webview's
// primary origin is the configured server host; while the page is on a FOREIGN
// host (an OIDC provider reached via top-level navigation) links stay inside,
// so the redirect chain completes. On the configured origin host, foreign-host
// clicks are externalized as usual.
#[tauri::command]
fn check_instance_flow(url: String) -> Result<bool, String> {
    let parsed: tauri::Url = url.parse().map_err(|e| format!("Invalid URL: {e}"))?;
    let host = parsed.host_str().unwrap_or("");
    let configured = CONFIGURED_ORIGIN_HOST.lock().map_err(|e| e.to_string())?;
    Ok(match configured.as_deref() {
        Some(origin) => host != origin,
        // Origin unknown (window not built yet), default to externalizing.
        None => false,
    })
}

#[tauri::command]
fn show_notification(app: tauri::AppHandle, title: String, body: String) -> Result<(), String> {
    // Check if notifications are enabled
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    let enabled = store
        .get("notifications_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !enabled {
        return Ok(());
    }

    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(&title)
        .body(&body)
        .show()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_notifications_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    Ok(store
        .get("notifications_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true))
}

#[tauri::command]
fn set_notifications_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    store.set("notifications_enabled", json!(enabled));
    store.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn open_settings(app: tauri::AppHandle) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("no main window")?;

    #[cfg(desktop)]
    let url = frontend_url("/?settings");
    #[cfg(mobile)]
    let url = {
        #[cfg(target_os = "android")]
        let base = "http://tauri.localhost";
        #[cfg(target_os = "ios")]
        let base = "tauri://localhost";
        format!("{base}/?settings")
            .parse::<tauri::Url>()
            .map_err(|e| e.to_string())?
    };

    window.navigate(url).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    // On Android/iOS the opener plugin opens the system browser.
    app.opener()
        .open_url(&url, None::<&str>)
        .map_err(|e| e.to_string())
}

// Forward the web app's Badging API count to the native dock/taskbar badge.
// Best-effort: no-op on Windows/Android where the platform lacks a badge API.
#[tauri::command]
fn set_badge(app: tauri::AppHandle, count: Option<i64>) -> Result<(), String> {
    // Treat 0 / negative as "clear" so the badge disappears when unread hits 0.
    let normalized = count.filter(|&c| c > 0);
    // set_badge_count only exists on the desktop WebviewWindow; mobile has no
    // dock/taskbar badge, so this is a no-op there.
    #[cfg(desktop)]
    if let Some(window) = app.get_webview_window("main") {
        return window.set_badge_count(normalized).map_err(|e| e.to_string());
    }
    #[cfg(not(desktop))]
    let _ = (&app, normalized);
    Ok(())
}

#[cfg(desktop)]
#[tauri::command]
fn get_autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    Ok(app.autolaunch().is_enabled().unwrap_or(false))
}

#[cfg(desktop)]
#[tauri::command]
fn set_autostart_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    if enabled {
        autolaunch.enable().map_err(|e| e.to_string())
    } else {
        autolaunch.disable().map_err(|e| e.to_string())
    }
}

// Query the update endpoint. Shared by the menu-driven check, the startup
// silent check, and the check_update/install_update IPC commands.
#[cfg(desktop)]
async fn fetch_pending_update(
    app: &tauri::AppHandle,
) -> Result<Option<tauri_plugin_updater::Update>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(|e| e.to_string())?;
    updater.check().await.map_err(|e| e.to_string())
}

// Returns Some(version) if an update is available, None if up to date.
#[cfg(desktop)]
#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<String>, String> {
    Ok(fetch_pending_update(&app).await?.map(|u| u.version.clone()))
}

// Download + install the pending update, then restart. Errors if none pending.
#[cfg(desktop)]
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    let update = fetch_pending_update(&app)
        .await?
        .ok_or("No update available")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart()
}

#[cfg(desktop)]
async fn do_update_check(app: tauri::AppHandle, silent: bool) {
    use tauri_plugin_notification::NotificationExt;

    let pending = match fetch_pending_update(&app).await {
        Ok(u) => u,
        Err(e) => {
            if !silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Update check failed")
                    .body(&e)
                    .show();
            }
            return;
        }
    };

    match pending {
        Some(update) => {
            if silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Chatto update available")
                    .body(&format!(
                        "v{} is ready, use Chatto > Check for Updates to install",
                        update.version
                    ))
                    .show();
            } else {
                let _ = app
                    .notification()
                    .builder()
                    .title(&format!("Downloading Chatto {}…", update.version))
                    .body("Chatto will restart when the update is ready.")
                    .show();
                match update.download_and_install(|_, _| {}, || {}).await {
                    Ok(_) => app.restart(),
                    Err(e) => {
                        let _ = app
                            .notification()
                            .builder()
                            .title("Update failed")
                            .body(&e.to_string())
                            .show();
                    }
                }
            }
        }
        None => {
            if !silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Chatto is up to date")
                    .body(&format!("v{} is the latest version.", app.package_info().version))
                    .show();
            }
        }
    }
}

#[cfg(desktop)]
fn frontend_url(path: &str) -> tauri::Url {
    #[cfg(debug_assertions)]
    let base = "http://localhost:1420";
    // Windows/Android use http://tauri.localhost, others use tauri://localhost
    #[cfg(all(not(debug_assertions), any(target_os = "windows", target_os = "android")))]
    let base = "http://tauri.localhost";
    #[cfg(all(not(debug_assertions), not(any(target_os = "windows", target_os = "android"))))]
    let base = "tauri://localhost";
    format!("{base}{path}")
        .parse()
        .expect("BUG: invalid frontend_url path")
}

#[cfg(desktop)]
fn navigate_to_settings(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.navigate(frontend_url("/?settings"));
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(desktop)]
fn toggle_window_visibility(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if window.is_visible().unwrap_or(false) {
            let _ = window.hide();
        } else {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
    }
}

#[cfg(desktop)]
fn setup_app_menu(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let about_metadata = AboutMetadataBuilder::new()
        .version(Some(env!("GIT_VERSION")))
        .website(Some("https://github.com/teal-bauer/chatto-tauri"))
        .website_label(Some("GitHub"))
        .license(Some("AGPL-3.0"))
        .build();

    let about = PredefinedMenuItem::about(app, Some("About Chatto"), Some(about_metadata))?;
    let sep = PredefinedMenuItem::separator(app)?;
    let check_updates = MenuItem::with_id(app, "menu_check_updates", "Check for Updates…", true, None::<&str>)?;
    let sep_updates = PredefinedMenuItem::separator(app)?;
    let settings = MenuItem::with_id(app, "menu_settings", "Settings…", true, Some("CmdOrCtrl+,"))?;
    let quit = PredefinedMenuItem::quit(app, None)?;

    #[cfg(target_os = "macos")]
    let app_submenu = {
        let hide = PredefinedMenuItem::hide(app, None)?;
        let hide_others = PredefinedMenuItem::hide_others(app, None)?;
        let show_all = PredefinedMenuItem::show_all(app, None)?;
        let sep2 = PredefinedMenuItem::separator(app)?;
        let sep3 = PredefinedMenuItem::separator(app)?;
        Submenu::with_items(
            app, "Chatto", true,
            &[&about, &sep, &check_updates, &sep_updates, &settings, &sep2, &hide, &hide_others, &show_all, &sep3, &quit],
        )?
    };

    #[cfg(not(target_os = "macos"))]
    let app_submenu = {
        let github = MenuItem::with_id(app, "menu_github", "GitHub…", true, None::<&str>)?;
        let sep2 = PredefinedMenuItem::separator(app)?;
        Submenu::with_items(
            app, "Chatto", true,
            &[&about, &github, &sep, &check_updates, &sep_updates, &settings, &sep2, &quit],
        )?
    };

    let edit_submenu = Submenu::with_items(
        app,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(app, None)?,
            &PredefinedMenuItem::redo(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, None)?,
            &PredefinedMenuItem::copy(app, None)?,
            &PredefinedMenuItem::paste(app, None)?,
            &PredefinedMenuItem::select_all(app, None)?,
        ],
    )?;

    let view_submenu = Submenu::with_items(
        app,
        "View",
        true,
        &[
            &MenuItem::with_id(app, "menu_back", "Back", true, Some("CmdOrCtrl+["))?,
            &MenuItem::with_id(app, "menu_forward", "Forward", true, Some("CmdOrCtrl+]"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_zoom_in", "Zoom In", true, Some("CmdOrCtrl+="))?,
            &MenuItem::with_id(app, "menu_zoom_out", "Zoom Out", true, Some("CmdOrCtrl+-"))?,
            &MenuItem::with_id(app, "menu_zoom_reset", "Actual Size", true, Some("CmdOrCtrl+0"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "menu_reload", "Reload", true, Some("CmdOrCtrl+R"))?,
        ],
    )?;

    let window_submenu = Submenu::with_items(
        app,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(app, None)?,
            &PredefinedMenuItem::maximize(app, None)?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::close_window(app, None)?,
        ],
    )?;

    let menu = Menu::with_items(
        app,
        &[&app_submenu, &edit_submenu, &view_submenu, &window_submenu],
    )?;

    menu.set_as_app_menu()?;

    // Handle custom menu events
    app.on_menu_event(move |app, event| match event.id().as_ref() {
        "menu_check_updates" => {
            let handle = app.clone();
            tauri::async_runtime::spawn(do_update_check(handle, false));
        }
        "menu_settings" => {
            navigate_to_settings(app);
        }
        "menu_github" => {
            use tauri_plugin_opener::OpenerExt;
            let _ = app.opener().open_url("https://github.com/teal-bauer/chatto-tauri", None::<&str>);
        }
        "menu_back" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval("history.back()");
            }
        }
        "menu_forward" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval("history.forward()");
            }
        }
        "menu_zoom_in" => {
            if let Some(window) = app.get_webview_window("main") {
                apply_zoom(&window, 10);
            }
        }
        "menu_zoom_out" => {
            if let Some(window) = app.get_webview_window("main") {
                apply_zoom(&window, -10);
            }
        }
        "menu_zoom_reset" => {
            if let Some(window) = app.get_webview_window("main") {
                apply_zoom(&window, 0);
            }
        }
        "menu_reload" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.eval("location.reload()");
            }
        }
        _ => {}
    });

    Ok(())
}

#[cfg(desktop)]
fn setup_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let show_hide = MenuItem::with_id(app, "show_hide", "Show/Hide", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;

    let autostart_enabled = {
        use tauri_plugin_autostart::ManagerExt;
        app.autolaunch().is_enabled().unwrap_or(false)
    };
    let autostart = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start at Login",
        true,
        autostart_enabled,
        None::<&str>,
    )?;

    let quit = MenuItem::with_id(app, "quit", "Quit Chatto", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[&show_hide, &settings, &separator, &autostart, &separator, &quit],
    )?;

    let icon = tauri::image::Image::from_bytes(TRAY_ICON_BYTES)?;

    let autostart_ref = autostart.clone();
    TrayIconBuilder::new()
        .icon(icon)
        .icon_as_template(true)
        .tooltip("Chatto")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show_hide" => toggle_window_visibility(app),
            "settings" => {
                navigate_to_settings(app);
            }
            "autostart" => {
                use tauri_plugin_autostart::ManagerExt;
                let autolaunch = app.autolaunch();
                let was_enabled = autolaunch.is_enabled().unwrap_or(false);
                let result = if was_enabled {
                    autolaunch.disable()
                } else {
                    autolaunch.enable()
                };
                if result.is_err() {
                    // Revert the auto-toggled checkbox state on failure
                    let _ = autostart_ref.set_checked(was_enabled);
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window_visibility(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// Forward a file drop from Explorer into the webview by injecting JS that
/// constructs File objects and dispatches dragenter/dragover/drop events.
#[cfg(target_os = "windows")]
fn forward_file_drop(
    window: &tauri::Window,
    paths: &[std::path::PathBuf],
    position: &tauri::PhysicalPosition<f64>,
) {
    use base64::Engine;

    let scale = window.scale_factor().unwrap_or(1.0);
    let lx = position.x / scale;
    let ly = position.y / scale;

    let mut files_js = String::new();
    for path in paths {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
        let name_js = name.replace('\\', "\\\\").replace('"', "\\\"");
        if !files_js.is_empty() {
            files_js.push(',');
        }
        files_js.push_str(&format!(
            r#"(()=>{{const s=atob("{b64}");const a=new Uint8Array(s.length);for(let i=0;i<s.length;i++)a[i]=s.charCodeAt(i);return new File([a],"{name_js}",{{type:"{mime}"}})}})()"#,
            b64 = b64,
            name_js = name_js,
            mime = mime
        ));
    }
    if files_js.is_empty() {
        return;
    }

    let script = format!(
        r#"(()=>{{
            const files=[{files_js}];
            const dt=new DataTransfer();
            files.forEach(f=>dt.items.add(f));
            const el=document.elementFromPoint({lx},{ly})||document.body;
            for(const t of['dragenter','dragover','drop']){{
                el.dispatchEvent(new DragEvent(t,{{bubbles:true,cancelable:true,dataTransfer:dt}}));
            }}
        }})();"#,
        files_js = files_js,
        lx = lx,
        ly = ly
    );
    if let Some(wv) = window.app_handle().get_webview_window(window.label()) {
        let _ = wv.eval(&script);
    }
}

fn get_server_url_from_store(app: &tauri::AppHandle) -> Option<String> {
    app.store("config.json")
        .ok()
        .and_then(|store| store.get("server_url").and_then(|v| v.as_str().map(String::from)))
}

// Translate a `chatto://` deep link into an https URL on the configured origin
// host, preserving path + query. The webview can only load http(s), so the raw
// custom-scheme URL would never load. Returns None for any non-`chatto` scheme.
fn deep_link_to_web_url(app: &tauri::AppHandle, url: &tauri::Url) -> Option<tauri::Url> {
    if url.scheme() != "chatto" {
        return None;
    }
    let origin = get_server_url_from_store(app).unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
    let origin_url: tauri::Url = origin.parse().ok()?;
    let scheme = origin_url.scheme();
    let authority = origin_url.authority(); // host[:port]

    // A custom-scheme URL splits the first segment into `host`; fold it back so
    // e.g. chatto://chat/-/room and chatto:///chat/-/room both round-trip.
    let mut path = String::new();
    if let Some(h) = url.host_str() {
        path.push_str(h);
    }
    path.push_str(url.path());
    let path = path.trim_start_matches('/');

    let mut target = format!("{scheme}://{authority}/{path}");
    if let Some(q) = url.query() {
        target.push('?');
        target.push_str(q);
    }
    target.parse().ok()
}

// Navigate the main window to a `chatto://` deep link's web equivalent.
fn handle_deep_link(app: &tauri::AppHandle, url: &tauri::Url) {
    match deep_link_to_web_url(app, url) {
        Some(target) => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.navigate(target);
                #[cfg(desktop)]
                {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        }
        None => eprintln!("rejected deep link with unexpected scheme: {}", url.scheme()),
    }
}

fn create_main_window(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let url = get_server_url_from_store(app.handle())
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());

    let webview_url = WebviewUrl::External(url.parse()?);

    let server_host = url
        .parse::<tauri::Url>()
        .ok()
        .and_then(|u| u.host_str().map(String::from));

    // Record the configured origin host so check_instance_flow can tell foreign
    // (OIDC) hosts from the primary origin on both desktop and mobile.
    if let Ok(mut guard) = CONFIGURED_ORIGIN_HOST.lock() {
        *guard = server_host.clone();
    }

    // External-link handling + OIDC flow runs on all platforms.
    let builder = WebviewWindowBuilder::new(app, "main", webview_url)
        .initialization_script(NOTIFICATION_BRIDGE_JS)
        .initialization_script(EXTERNAL_LINK_JS);

    #[cfg(mobile)]
    let builder = builder
        .initialization_script(MOBILE_VIEWPORT_FIT_JS)
        .initialization_script(MOBILE_SETTINGS_BUTTON_JS);

    #[cfg(target_os = "android")]
    let builder = builder
        .initialization_script(KEYBOARD_VIEWPORT_SHIM_JS)
        .initialization_script(DIALOG_KEYBOARD_GUARD_JS)
        .initialization_script(ACTIVE_ROOM_TRACKER_JS);

    #[cfg(desktop)]
    let builder = {
        let server_host_clone = server_host.clone();
        let app_handle = app.handle().clone();
        builder
            .title("Chatto")
            .inner_size(1024.0, 768.0)
            .min_inner_size(400.0, 300.0)
            .zoom_hotkeys_enabled(true)
            .on_document_title_changed(|window, title| {
                let _ = window.set_title(&title);
            })
            .on_navigation(move |_url| {
                // Allow all navigations, EXTERNAL_LINK_JS handles opening
                // external links in the system browser for user-initiated clicks.
                // Intercepting here breaks iframe embeds (YouTube, etc.) because
                // on_navigation fires for subframe loads too.
                let _ = (&server_host_clone, &app_handle); // suppress unused warnings
                true
            })
    };

    let window = builder.build()?;
    // window is only read by the desktop zoom-restore block below.
    #[cfg(not(desktop))]
    let _ = &window;

    // Restore persisted zoom level
    #[cfg(desktop)]
    if let Ok(store) = app.handle().store("config.json") {
        if let Some(level) = store.get("zoom_level").and_then(|v| v.as_i64()) {
            let level = (level as i32).clamp(30, 300);
            ZOOM_LEVEL.store(level, Ordering::SeqCst);
            let _ = window.set_zoom(level as f64 / 100.0);
        }
    }

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        set_server_url,
        get_server_url,
        clear_server_url,
        open_settings,
        open_external_url,
        show_notification,
        get_notifications_enabled,
        set_notifications_enabled,
        get_autostart_enabled,
        set_autostart_enabled,
        check_instance_flow,
        set_badge,
        check_update,
        install_update,
    ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        set_server_url,
        get_server_url,
        clear_server_url,
        open_settings,
        open_external_url,
        show_notification,
        get_notifications_enabled,
        set_notifications_enabled,
        check_instance_flow,
        set_badge,
    ]);

    let builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            // Autostart
            #[cfg(desktop)]
            {
                use tauri_plugin_autostart::MacosLauncher;
                app.handle().plugin(tauri_plugin_autostart::init(
                    MacosLauncher::LaunchAgent,
                    None,
                ))?;
            }

            // Window state persistence
            #[cfg(desktop)]
            app.handle()
                .plugin(tauri_plugin_window_state::Builder::default().build())?;

            // Auto-updater
            #[cfg(desktop)]
            app.handle()
                .plugin(tauri_plugin_updater::Builder::new().build())?;

            // Deep links
            #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
            app.deep_link().register_all()?;

            // Runtime deep links (app already running). Translate chatto:// to the
            // web equivalent and navigate the main window.
            let app_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                let urls = event.urls();
                eprintln!("deep link opened: {:?}", urls);
                if let Some(url) = urls.first() {
                    handle_deep_link(&app_handle, url);
                }
            });

            // macOS menu bar
            #[cfg(desktop)]
            setup_app_menu(app)?;

            // System tray
            #[cfg(desktop)]
            setup_tray(app)?;

            // Create main window
            create_main_window(app)?;

            // Cold-start deep link: the app was launched by a chatto:// URL.
            // Handle after the window exists so navigation lands.
            if let Some(urls) = app.deep_link().get_current()? {
                eprintln!("launched via deep link: {:?}", urls);
                if let Some(url) = urls.first() {
                    handle_deep_link(app.handle(), url);
                }
            }

            // Background update check on startup
            #[cfg(desktop)]
            {
                let update_handle = app.handle().clone();
                tauri::async_runtime::spawn(do_update_check(update_handle, true));
            }

            Ok(())
        });

    // Close-to-tray on desktop only; Windows file-drop forwarding
    #[cfg(desktop)]
    let builder = builder.on_window_event(|window, event| {
        match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                let _ = window.hide();
                api.prevent_close();
            }
            tauri::WindowEvent::Focused(focused) => {
                let js = if *focused {
                    "window.__chattoWindowHidden=false;document.dispatchEvent(new Event('visibilitychange'));"
                } else {
                    "window.__chattoWindowHidden=true;document.dispatchEvent(new Event('visibilitychange'));"
                };
                if let Some(wv) = window.app_handle().get_webview_window(window.label()) {
                    let _ = wv.eval(js);
                }
            }
            #[cfg(target_os = "windows")]
            tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, position }) => {
                forward_file_drop(window, paths, position);
            }
            _ => {}
        }
    });

    builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // On macOS, re-show the main window when the app is re-activated
            // (dock icon clicked, or brought to front after a notification click).
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows, .. } = event {
                if !has_visible_windows {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
            let _ = (app, event);
        });
}
