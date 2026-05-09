package run.chatto.desktop

import android.content.Intent
import android.content.res.Configuration
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.View
import android.view.ViewGroup
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.widget.FrameLayout
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
      android.graphics.drawable.ColorDrawable(
        if (isDark) Color.parseColor("#171717") else Color.WHITE
      )
    )

    updateStatusBarTheme(resources.configuration)

    // Request notification permission on Android 13+
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 0)
    }

    handleNavigateIntent(intent)
  }

  /** Wry calls this once the WebView has been instantiated and added to the view hierarchy. */
  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    Log.d(TAG, "onWebViewCreate: webView=${webView.javaClass.simpleName} parent=${webView.parent?.javaClass?.simpleName}")
    rustWebView = webView
    webView.addJavascriptInterface(ChattoJsBridge(), "ChattoAndroid")

    val isDark = (resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
      Configuration.UI_MODE_NIGHT_YES
    webView.setBackgroundColor(if (isDark) Color.parseColor("#171717") else Color.WHITE)

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
      Log.d(TAG, "insets: sys=$sys ime=$ime effectiveBottom=$bottom parent=${v.parent?.javaClass?.simpleName} lpClass=${v.layoutParams?.javaClass?.simpleName}")

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
