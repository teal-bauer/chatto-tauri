use tauri::{Emitter, Manager};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_store::StoreExt;

#[cfg(desktop)]
use tauri::{
    menu::{AboutMetadataBuilder, CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri::{WebviewUrl, WebviewWindowBuilder};

use serde_json::json;
use std::sync::atomic::{AtomicI32, Ordering};

const NOTIFICATION_BRIDGE_JS: &str = r#"
(function() {
    if (window.__chattoNotificationBridged) return;
    window.__chattoNotificationBridged = true;

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

#[cfg(mobile)]
const MOBILE_SETTINGS_BUTTON_JS: &str = r#"
(function() {
    if (window.__chattoSettingsButton) return;
    window.__chattoSettingsButton = true;

    function createButton() {
        if (document.getElementById('chatto-settings-btn')) return;
        var btn = document.createElement('button');
        btn.id = 'chatto-settings-btn';
        btn.innerHTML = '&#9881;';
        btn.style.cssText = 'position:fixed;bottom:16px;right:16px;z-index:999999;' +
            'width:44px;height:44px;border-radius:50%;border:none;' +
            'background:rgba(99,102,241,0.9);color:white;font-size:22px;' +
            'cursor:pointer;box-shadow:0 2px 8px rgba(0,0,0,0.3);' +
            'display:flex;align-items:center;justify-content:center;' +
            '-webkit-tap-highlight-color:transparent;';
        btn.addEventListener('click', function() {
            if (window.__TAURI_INTERNALS__) {
                window.__TAURI_INTERNALS__.invoke('open_settings').catch(function() {});
            }
        });
        document.body.appendChild(btn);
    }

    if (document.body) createButton();
    else document.addEventListener('DOMContentLoaded', createButton);
    new MutationObserver(function() { if (document.body) createButton(); })
        .observe(document.documentElement, { childList: true });
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
                    ureq::ErrorKind::Dns => "Server not found — check the address",
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

#[cfg(desktop)]
#[tauri::command]
fn clear_server_url(app: tauri::AppHandle) -> Result<(), String> {
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    store.delete("server_url");
    store.save().map_err(|e| e.to_string())?;

    let window = app.get_webview_window("main").ok_or("no main window")?;
    window.navigate(frontend_url("/")).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_server_url(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let store = app.store("config.json").map_err(|e| e.to_string())?;
    Ok(store
        .get("server_url")
        .and_then(|v| v.as_str().map(|s| s.to_string())))
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
            .parse()
            .map_err(|e| e.to_string())?
    };

    window.navigate(url).map_err(|e| e.to_string())
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

#[cfg(desktop)]
async fn do_update_check(app: tauri::AppHandle, silent: bool) {
    use tauri_plugin_notification::NotificationExt;
    use tauri_plugin_updater::UpdaterExt;

    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            if !silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Update check failed")
                    .body(&e.to_string())
                    .show();
            }
            return;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            if silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Chatto update available")
                    .body(&format!(
                        "v{} is ready — use Chatto > Check for Updates to install",
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
        Ok(None) => {
            if !silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Chatto is up to date")
                    .body(&format!("v{} is the latest version.", app.package_info().version))
                    .show();
            }
        }
        Err(e) => {
            if !silent {
                let _ = app
                    .notification()
                    .builder()
                    .title("Update check failed")
                    .body(&e.to_string())
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

fn get_server_url_from_store(app: &tauri::AppHandle) -> Option<String> {
    app.store("config.json")
        .ok()
        .and_then(|store| store.get("server_url").and_then(|v| v.as_str().map(String::from)))
}

fn create_main_window(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let url = get_server_url_from_store(app.handle());

    let webview_url = match &url {
        Some(u) => WebviewUrl::External(u.parse()?),
        None => WebviewUrl::default(),
    };

    let server_host = url
        .as_ref()
        .and_then(|u| u.parse::<tauri::Url>().ok())
        .and_then(|u| u.host_str().map(String::from));

    let builder = WebviewWindowBuilder::new(app, "main", webview_url)
        .initialization_script(NOTIFICATION_BRIDGE_JS);

    #[cfg(mobile)]
    let builder = builder.initialization_script(MOBILE_SETTINGS_BUTTON_JS);

    #[cfg(desktop)]
    let builder = {
        let server_host_clone = server_host.clone();
        let app_handle = app.handle().clone();
        builder
            .title("Chatto")
            .inner_size(1024.0, 768.0)
            .min_inner_size(400.0, 300.0)
            .zoom_hotkeys_enabled(true)
            .disable_drag_drop_handler()
            .on_document_title_changed(|window, title| {
                let _ = window.set_title(&title);
            })
            .on_navigation(move |url| {
                let navigating_host = url.host_str().unwrap_or_default();
                if url.scheme() == "tauri"
                    || url.scheme() == "about"
                    || navigating_host == "localhost"
                    || navigating_host == "tauri.localhost"
                    || server_host_clone
                        .as_ref()
                        .map(|h| navigating_host == h.as_str())
                        .unwrap_or(false)
                {
                    return true;
                }
                use tauri_plugin_opener::OpenerExt;
                let _ = app_handle.opener().open_url(url.as_str(), None::<&str>);
                false
            })
    };

    let window = builder.build()?;

    // Restore persisted zoom level
    #[cfg(desktop)]
    if let Ok(store) = app.handle().store("config.json") {
        if let Some(level) = store.get("zoom_level").and_then(|v| v.as_i64()) {
            let level = (level as i32).clamp(30, 300);
            ZOOM_LEVEL.store(level, Ordering::SeqCst);
            let _ = window.set_zoom(level as f64 / 100.0);
        }
    }

    if url.is_none() {
        let _ = app.handle().emit("open-settings", ());
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
        show_notification,
        get_notifications_enabled,
        set_notifications_enabled,
        get_autostart_enabled,
        set_autostart_enabled,
    ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        set_server_url,
        get_server_url,
        open_settings,
        show_notification,
        get_notifications_enabled,
        set_notifications_enabled,
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

            if let Some(urls) = app.deep_link().get_current()? {
                eprintln!("launched via deep link: {:?}", urls);
            }

            let app_handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                let urls = event.urls();
                eprintln!("deep link opened: {:?}", urls);
                if let Some(url) = urls.first() {
                    if url.scheme() != "chatto" {
                        eprintln!("rejected deep link with unexpected scheme: {}", url.scheme());
                        return;
                    }
                    if let Some(window) = app_handle.get_webview_window("main") {
                        let _ = window.navigate(url.clone());
                    }
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

            // Background update check on startup
            #[cfg(desktop)]
            {
                let update_handle = app.handle().clone();
                tauri::async_runtime::spawn(do_update_check(update_handle, true));
            }

            Ok(())
        });

    // Close-to-tray on desktop only
    #[cfg(desktop)]
    let builder = builder.on_window_event(|window, event| {
        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
            let _ = window.hide();
            api.prevent_close();
        }
    });

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
