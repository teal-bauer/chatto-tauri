package run.chatto.desktop

import android.content.Intent
import android.content.res.Configuration
import android.graphics.Color
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import android.view.ViewGroup
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.widget.FrameLayout
import androidx.activity.OnBackPressedCallback
import androidx.core.splashscreen.SplashScreen.Companion.installSplashScreen
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat

class MainActivity : TauriActivity() {

  companion object {
    private const val TAG = "ChattoMainActivity"
  }

  /** Exposed to JS as window.ChattoAndroid */
  private inner class ChattoJsBridge {
    @JavascriptInterface
    fun setActiveRoom(roomId: String) {
      NotificationService.activeRoomId = roomId.ifBlank { null }
    }
  }

  /** Captured in onWebViewCreate so we don't have to walk the view tree to find it. */
  private var rustWebView: WebView? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    // Show the Android 12+ splash (backwards-compatible) before the content is
    // ready, so cold start doesn't flash a blank frame. Must run before super.
    installSplashScreen()
    super.onCreate(savedInstanceState)

    // True edge-to-edge — required on Android 15+ (SDK 35+) and the modern
    // recommended pattern. The system bars are transparent; we resize the
    // WebView via margins so its layout viewport (and any position:fixed
    // children) stays inside the safe area.
    WindowCompat.setDecorFitsSystemWindows(window, false)
    window.statusBarColor = Color.TRANSPARENT
    window.navigationBarColor = Color.TRANSPARENT
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      window.isNavigationBarContrastEnforced = false
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
      window.attributes.layoutInDisplayCutoutMode =
        android.view.WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES
    }

    // Match the activity background to system-bar colors so the visible bands
    // behind the transparent status bar / gesture handle aren't black.
    val isDark = (resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
      Configuration.UI_MODE_NIGHT_YES
    window.setBackgroundDrawable(
      android.graphics.drawable.ColorDrawable(backgroundColor(isDark))
    )

    updateStatusBarTheme(resources.configuration)

    // Request notification permission on Android 13+
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 0)
    }

    installBackHandler()
    handleNavigateIntent(intent)
  }

  /** Single source of truth for the window/WebView background (shared with themes). */
  private fun backgroundColor(isDark: Boolean): Int =
    getColor(if (isDark) R.color.chatto_background_dark else R.color.chatto_background_light)

  /**
   * Back button navigates WebView history when possible (chat SPA), otherwise
   * falls through to the default (leave the activity). TauriActivity sets
   * handleBackNavigation=false, so the WebView-aware handler lives here.
   */
  private fun installBackHandler() {
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        val wv = rustWebView
        if (wv != null && wv.canGoBack()) {
          wv.goBack()
        } else {
          isEnabled = false
          onBackPressedDispatcher.onBackPressed()
          isEnabled = true
        }
      }
    })
  }

  /** Wry calls this once the WebView has been instantiated and added to the view hierarchy. */
  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    if (BuildConfig.DEBUG) {
      Log.d(TAG, "onWebViewCreate: webView=${webView.javaClass.simpleName} parent=${webView.parent?.javaClass?.simpleName}")
    }
    rustWebView = webView
    webView.addJavascriptInterface(ChattoJsBridge(), "ChattoAndroid")

    val isDark = (resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
      Configuration.UI_MODE_NIGHT_YES
    webView.setBackgroundColor(backgroundColor(isDark))

    installInsetsListener(webView)
  }

  private fun installInsetsListener(webView: WebView) {
    // Forward system-bar + IME insets to JS, and resize the WebView to the
    // safe area via layout margins. Bottom margin is max(systemBars, IME) so
    // that:
    //   - keyboard hidden → bottom margin = gesture handle height
    //   - keyboard shown  → bottom margin = keyboard height (which already
    //     subsumes the gesture handle)
    // The page never has to know about either; position:fixed bottom elements
    // stay visible above whatever is taking up the bottom of the screen.
    ViewCompat.setOnApplyWindowInsetsListener(webView) { v, insets ->
      val sys = insets.getInsets(WindowInsetsCompat.Type.systemBars())
      val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
      val bottom = maxOf(sys.bottom, ime.bottom)
      if (BuildConfig.DEBUG) {
        Log.d(TAG, "insets: sys=$sys ime=$ime effectiveBottom=$bottom parent=${v.parent?.javaClass?.simpleName} lpClass=${v.layoutParams?.javaClass?.simpleName}")
      }

      val parent = v.parent as? ViewGroup
      // Replace whatever LayoutParams Wry set up with a FrameLayout-compatible
      // one that supports margins. android.R.id.content is a FrameLayout.
      val cur = v.layoutParams
      val newLp = if (parent is FrameLayout) {
        FrameLayout.LayoutParams(
          ViewGroup.LayoutParams.MATCH_PARENT,
          ViewGroup.LayoutParams.MATCH_PARENT
        ).also { it.setMargins(sys.left, sys.top, sys.right, bottom) }
      } else if (cur is ViewGroup.MarginLayoutParams) {
        cur.also { it.setMargins(sys.left, sys.top, sys.right, bottom) }
      } else {
        ViewGroup.MarginLayoutParams(cur ?: ViewGroup.LayoutParams(
          ViewGroup.LayoutParams.MATCH_PARENT,
          ViewGroup.LayoutParams.MATCH_PARENT
        )).also { it.setMargins(sys.left, sys.top, sys.right, bottom) }
      }
      v.layoutParams = newLp

      val wv = v as? WebView
      val density = resources.displayMetrics.density
      val js = """
        (function(){
          var i = window.__chattoInsets = window.__chattoInsets || {};
          i.top = ${sys.top / density};
          i.right = ${sys.right / density};
          i.bottom = ${sys.bottom / density};
          i.left = ${sys.left / density};
          i.keyboard = ${ime.bottom / density};
          var r = document.documentElement;
          if (r && r.style) {
            // The WebView is sized to the safe area, so env(safe-area-inset-*)
            // is 0 from the page's perspective. CSS vars are still set in case
            // anything wants the actual device values.
            r.style.setProperty('--chatto-inset-top', '0px');
            r.style.setProperty('--chatto-inset-right', '0px');
            r.style.setProperty('--chatto-inset-bottom', '0px');
            r.style.setProperty('--chatto-inset-left', '0px');
            r.style.setProperty('--chatto-keyboard-inset', i.keyboard + 'px');
          }
          if (typeof window.__chattoOnInsets === 'function') {
            try { window.__chattoOnInsets(i); } catch(_){}
          }
        })();
      """.trimIndent()
      wv?.evaluateJavascript(js, null)

      // Consume the system bar insets — children don't need them since we've
      // already moved out of their reach. Pass IME through.
      WindowInsetsCompat.Builder(insets)
        .setInsets(WindowInsetsCompat.Type.systemBars(), androidx.core.graphics.Insets.NONE)
        .build()
    }
    ViewCompat.requestApplyInsets(webView)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    handleNavigateIntent(intent)
  }

  override fun onResume() {
    super.onResume()
    NotificationService.start(this)
    setVisibilityInJs(false)
    maybeRequestBatteryExemption()
  }

  /**
   * The background WebSocket is fragile under Doze. Ask once (persisted) to be
   * exempted from battery optimizations; never nag after the first prompt.
   */
  private fun maybeRequestBatteryExemption() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.M) return
    val prefs = getSharedPreferences("chatto_prefs", MODE_PRIVATE)
    if (prefs.getBoolean("battery_prompt_shown", false)) return

    val pm = getSystemService(PowerManager::class.java)
    if (pm != null && pm.isIgnoringBatteryOptimizations(packageName)) return

    prefs.edit().putBoolean("battery_prompt_shown", true).apply()
    try {
      val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS).apply {
        data = Uri.parse("package:$packageName")
      }
      startActivity(intent)
    } catch (e: Exception) {
      Log.w(TAG, "Battery exemption request failed: ${e.message}")
    }
  }

  override fun onPause() {
    super.onPause()
    NotificationService.activeRoomId = null
    setVisibilityInJs(true)
  }

  private fun setVisibilityInJs(hidden: Boolean) {
    val webView = rustWebView ?: return
    val js = if (hidden) {
      "window.__chattoWindowHidden=true;document.dispatchEvent(new Event('visibilitychange'));"
    } else {
      "window.__chattoWindowHidden=false;document.dispatchEvent(new Event('visibilitychange'));"
    }
    webView.evaluateJavascript(js, null)
  }

  private fun handleNavigateIntent(intent: Intent?) {
    val url = intent?.getStringExtra("navigate_url") ?: return
    intent.removeExtra("navigate_url") // consume it
    window.decorView.postDelayed({
      rustWebView?.evaluateJavascript("window.location.href=${jsString(url)}", null)
    }, 500)
  }

  private fun jsString(s: String): String {
    val escaped = s.replace("\\", "\\\\").replace("'", "\\'").replace("\n", "\\n")
    return "'$escaped'"
  }

  override fun onConfigurationChanged(newConfig: Configuration) {
    super.onConfigurationChanged(newConfig)
    updateStatusBarTheme(newConfig)
  }

  private fun updateStatusBarTheme(config: Configuration) {
    val isDarkMode = (config.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
      Configuration.UI_MODE_NIGHT_YES
    val controller = WindowInsetsControllerCompat(window, window.decorView)
    controller.isAppearanceLightStatusBars = !isDarkMode
    controller.isAppearanceLightNavigationBars = !isDarkMode
  }
}
