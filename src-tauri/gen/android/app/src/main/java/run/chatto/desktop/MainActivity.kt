package run.chatto.desktop

import android.content.res.Configuration
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsControllerCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    // Opt out of edge-to-edge — let the system handle keyboard resize
    WindowCompat.setDecorFitsSystemWindows(window, true)
    updateStatusBarTheme(resources.configuration)

    // Request notification permission on Android 13+
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 0)
    }
  }

  override fun onResume() {
    super.onResume()
    // Start the background notification service — it will connect once
    // the user is logged in and cookies are available
    NotificationService.start(this)
  }

  override fun onConfigurationChanged(newConfig: Configuration) {
    super.onConfigurationChanged(newConfig)
    updateStatusBarTheme(newConfig)
  }

  private fun updateStatusBarTheme(config: Configuration) {
    val isDarkMode = (config.uiMode and Configuration.UI_MODE_NIGHT_MASK) ==
      Configuration.UI_MODE_NIGHT_YES
    val controller = WindowInsetsControllerCompat(window, window.decorView)
    // Light status bar = dark icons (for light backgrounds)
    controller.isAppearanceLightStatusBars = !isDarkMode
    window.statusBarColor = if (isDarkMode) Color.parseColor("#171717") else Color.WHITE
  }
}
