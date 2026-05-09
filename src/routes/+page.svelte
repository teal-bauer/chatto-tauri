<script lang="ts">
  import { onMount, onDestroy } from "svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";

  let serverUrl = $state("");
  let hasOverride = $state(false);
  let error = $state("");
  let loading = $state(true);
  let connecting = $state(false);
  let showSettings = $state(false);
  let notificationsEnabled = $state(true);
  let autostartEnabled = $state(false);
  let autostartAvailable = $state(false);

  let unlisten: UnlistenFn | undefined;

  onMount(async () => {
    const params = new URLSearchParams(window.location.search);
    showSettings = params.has("settings");

    unlisten = await listen("open-settings", () => {
      showSettings = true;
      connecting = false;
      loadPreferences();
    });

    try {
      const url = await invoke<string | null>("get_server_url");
      hasOverride = !!url;
      serverUrl = url ?? "";
      if (showSettings) {
        await loadPreferences();
      }
    } catch {
      showSettings = true;
    }
    loading = false;
  });

  onDestroy(() => {
    unlisten?.();
  });

  async function loadPreferences() {
    try {
      notificationsEnabled = await invoke<boolean>("get_notifications_enabled");
    } catch {
      // defaults are fine
    }
    try {
      autostartEnabled = await invoke<boolean>("get_autostart_enabled");
      autostartAvailable = true;
    } catch {
      // autostart not available (mobile)
      autostartAvailable = false;
    }
  }

  async function connect(event?: Event) {
    event?.preventDefault();
    error = "";

    let url = serverUrl.trim() || "https://chat.chatto.run";

    if (!/^https?:\/\//i.test(url)) {
      url = "https://" + url;
    }

    try {
      new URL(url);
    } catch {
      error = "Invalid URL format.";
      return;
    }

    connecting = true;

    try {
      await invoke("set_server_url", { url });
      hasOverride = true;
      // The webview will navigate to the server URL — this UI disappears
    } catch (e) {
      error = `${e}`;
      connecting = false;
    }
  }

  async function resetToDefault() {
    error = "";
    connecting = true;
    try {
      await invoke("clear_server_url");
      hasOverride = false;
      serverUrl = "";
    } catch (e) {
      error = `${e}`;
    }
    connecting = false;
  }

  async function toggleNotifications() {
    notificationsEnabled = !notificationsEnabled;
    try {
      await invoke("set_notifications_enabled", { enabled: notificationsEnabled });
    } catch (e) {
      notificationsEnabled = !notificationsEnabled;
      error = `Failed to update notifications: ${e}`;
    }
  }

  async function toggleAutostart() {
    autostartEnabled = !autostartEnabled;
    try {
      await invoke("set_autostart_enabled", { enabled: autostartEnabled });
    } catch (e) {
      autostartEnabled = !autostartEnabled;
      error = `Failed to update autostart: ${e}`;
    }
  }
</script>

{#if loading}
  <main class="splash">
    <img src="/icon.png" alt="Chatto" class="icon icon-pulse" width="96" height="96" />
  </main>
{:else if showSettings || connecting}
  <main class="page">
    <div class="scroll">
      <header class="hero">
        <img src="/icon.png" alt="Chatto" class="icon" width="72" height="72" />
        <h1>Chatto</h1>
        <p class="subtitle">App Settings</p>
      </header>

      <form id="settings-form" class="settings" onsubmit={connect}>
        <section class="card">
          <h2>Server</h2>
          <label class="field">
            <span class="field-label">Address</span>
            <input
              type="url"
              inputmode="url"
              bind:value={serverUrl}
              placeholder="chat.chatto.run"
              spellcheck="false"
              autocomplete="off"
              autocapitalize="off"
              autocorrect="off"
              disabled={connecting}
            />
          </label>
          {#if hasOverride}
            <button type="button" class="link-btn" onclick={resetToDefault} disabled={connecting}>
              Reset to default server
            </button>
          {/if}
        </section>

        <section class="card">
          <h2>Preferences</h2>
          <label class="toggle-row">
            <span class="toggle-label">
              <span class="toggle-title">Notifications</span>
              <span class="toggle-hint">Native alerts when messages arrive while Chatto is in the background.</span>
            </span>
            <button
              type="button"
              class="toggle"
              class:active={notificationsEnabled}
              onclick={toggleNotifications}
              role="switch"
              aria-checked={notificationsEnabled}
              aria-label="Toggle notifications"
            >
              <span class="toggle-knob"></span>
            </button>
          </label>
          {#if autostartAvailable}
            <label class="toggle-row">
              <span class="toggle-label">
                <span class="toggle-title">Start at Login</span>
                <span class="toggle-hint">Launch Chatto automatically when you sign in.</span>
              </span>
              <button
                type="button"
                class="toggle"
                class:active={autostartEnabled}
                onclick={toggleAutostart}
                role="switch"
                aria-checked={autostartEnabled}
                aria-label="Toggle start at login"
              >
                <span class="toggle-knob"></span>
              </button>
            </label>
          {/if}
        </section>

        {#if error}
          <p class="error" role="alert">{error}</p>
        {/if}

        {#if !showSettings && connecting}
          <p class="status">Connecting…</p>
        {/if}
      </form>
    </div>

    <div class="action-bar">
      <button type="submit" form="settings-form" disabled={connecting} class="primary">
        {connecting ? "Connecting…" : hasOverride ? "Update & Reload" : "Connect"}
      </button>
    </div>
  </main>
{/if}

<style>
  :root {
    --bg: #fafafa;
    --bg-elev: #ffffff;
    --fg: #1a1a1a;
    --fg-muted: #666;
    --fg-faint: #999;
    --border: #e6e6e6;
    --border-strong: #ccc;
    --accent: #6366f1;
    --accent-hover: #4f46e5;
    --error: #ef4444;
    --shadow: 0 1px 2px rgba(0, 0, 0, 0.05), 0 4px 12px rgba(0, 0, 0, 0.04);
    --inset-top: max(env(safe-area-inset-top), var(--chatto-inset-top, 0px));
    --inset-right: max(env(safe-area-inset-right), var(--chatto-inset-right, 0px));
    --inset-bottom: max(env(safe-area-inset-bottom), var(--chatto-inset-bottom, 0px));
    --inset-left: max(env(safe-area-inset-left), var(--chatto-inset-left, 0px));

    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    font-size: 16px;
    color: var(--fg);
    background: var(--bg);
    -webkit-text-size-adjust: 100%;
  }

  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0d0d0d;
      --bg-elev: #1a1a1a;
      --fg: #e8e8e8;
      --fg-muted: #a0a0a0;
      --fg-faint: #777;
      --border: #2a2a2a;
      --border-strong: #444;
      --shadow: 0 1px 2px rgba(0, 0, 0, 0.4), 0 4px 12px rgba(0, 0, 0, 0.3);
    }
  }

  :global(html, body) {
    margin: 0;
    padding: 0;
    background: var(--bg);
    color: var(--fg);
    min-height: 100dvh;
  }

  :global(html) {
    overscroll-behavior: contain;
  }

  .splash {
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 100dvh;
    padding: var(--inset-top) var(--inset-right) var(--inset-bottom) var(--inset-left);
  }

  .icon {
    border-radius: 22%;
    box-shadow: var(--shadow);
  }

  .icon-pulse {
    animation: pulse 2s ease-in-out infinite;
  }

  @keyframes pulse {
    0%, 100% { opacity: 1; transform: scale(1); }
    50% { opacity: 0.7; transform: scale(0.96); }
  }

  .page {
    display: flex;
    flex-direction: column;
    min-height: 100dvh;
    background: var(--bg);
  }

  .scroll {
    flex: 1 1 auto;
    overflow-y: auto;
    -webkit-overflow-scrolling: touch;
    padding:
      calc(var(--inset-top) + 1.5rem)
      calc(var(--inset-right) + 1.25rem)
      1.5rem
      calc(var(--inset-left) + 1.25rem);
  }

  .hero {
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
    margin-bottom: 1.75rem;
  }

  .hero .icon {
    margin-bottom: 0.875rem;
  }

  h1 {
    font-size: 1.75rem;
    font-weight: 600;
    margin: 0;
    letter-spacing: -0.01em;
  }

  .subtitle {
    color: var(--fg-muted);
    margin: 0.25rem 0 0;
    font-size: 0.95rem;
  }

  .settings {
    width: 100%;
    max-width: 540px;
    margin: 0 auto;
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }

  .card {
    background: var(--bg-elev);
    border: 1px solid var(--border);
    border-radius: 14px;
    padding: 1rem 1.125rem;
    box-shadow: var(--shadow);
  }

  h2 {
    font-size: 0.7rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--fg-muted);
    margin: 0 0 0.75rem;
  }

  .field {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
  }

  .field-label {
    font-size: 0.875rem;
    color: var(--fg-muted);
  }

  input {
    width: 100%;
    box-sizing: border-box;
    padding: 0.875rem 0.875rem;
    border: 1px solid var(--border-strong);
    border-radius: 10px;
    font-size: 1rem;
    background: var(--bg);
    color: var(--fg);
    outline: none;
    transition: border-color 0.15s, box-shadow 0.15s;
    min-height: 48px;
  }

  input:focus {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 25%, transparent);
  }

  input:disabled {
    opacity: 0.6;
  }

  .link-btn {
    align-self: flex-start;
    margin-top: 0.625rem;
    background: none;
    border: none;
    padding: 0.375rem 0;
    color: var(--accent);
    font-size: 0.875rem;
    cursor: pointer;
    min-height: 32px;
  }

  .link-btn:hover:not(:disabled) {
    text-decoration: underline;
  }

  .link-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .toggle-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.75rem 0;
    border-bottom: 1px solid var(--border);
    cursor: pointer;
    min-height: 48px;
  }

  .toggle-row:first-of-type {
    padding-top: 0;
  }

  .toggle-row:last-of-type {
    padding-bottom: 0;
    border-bottom: none;
  }

  .toggle-label {
    display: flex;
    flex-direction: column;
    gap: 0.125rem;
    flex: 1;
    min-width: 0;
  }

  .toggle-title {
    font-size: 0.95rem;
    font-weight: 500;
    color: var(--fg);
  }

  .toggle-hint {
    font-size: 0.8rem;
    color: var(--fg-faint);
    line-height: 1.35;
  }

  .toggle {
    flex-shrink: 0;
    position: relative;
    width: 48px;
    height: 28px;
    border-radius: 14px;
    border: none;
    background: var(--border-strong);
    cursor: pointer;
    padding: 0;
    transition: background 0.2s;
  }

  .toggle.active {
    background: var(--accent);
  }

  .toggle-knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 24px;
    height: 24px;
    border-radius: 50%;
    background: white;
    transition: transform 0.2s;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.2);
  }

  .toggle.active .toggle-knob {
    transform: translateX(20px);
  }

  .error {
    color: var(--error);
    margin: 0;
    padding: 0.75rem 1rem;
    background: color-mix(in srgb, var(--error) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--error) 35%, transparent);
    border-radius: 10px;
    font-size: 0.875rem;
  }

  .status {
    text-align: center;
    color: var(--fg-muted);
    margin: 0;
  }

  .action-bar {
    flex-shrink: 0;
    padding:
      0.75rem
      calc(var(--inset-right) + 1.25rem)
      calc(var(--inset-bottom) + 0.75rem)
      calc(var(--inset-left) + 1.25rem);
    background: var(--bg);
    border-top: 1px solid var(--border);
    display: flex;
    justify-content: center;
  }

  .primary {
    width: 100%;
    max-width: 540px;
    min-height: 50px;
    padding: 0 1.5rem;
    background: var(--accent);
    color: white;
    border: none;
    border-radius: 12px;
    font-size: 1rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.15s, opacity 0.15s, transform 0.05s;
  }

  .primary:hover:not(:disabled) {
    background: var(--accent-hover);
  }

  .primary:active:not(:disabled) {
    transform: scale(0.98);
  }

  .primary:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Wider screens: keep the card layout but cap width and center vertically. */
  @media (min-width: 720px) and (min-height: 600px) {
    .scroll {
      padding-top: calc(var(--inset-top) + 3rem);
      padding-bottom: 2rem;
    }
    .hero {
      margin-bottom: 2rem;
    }
    h1 {
      font-size: 2rem;
    }
    .action-bar {
      border-top: none;
      background: transparent;
      padding-top: 0;
      padding-bottom: calc(var(--inset-bottom) + 2rem);
    }
  }
</style>
