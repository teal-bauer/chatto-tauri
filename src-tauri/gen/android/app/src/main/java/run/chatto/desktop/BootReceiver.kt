package run.chatto.desktop

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * Restarts the foreground NotificationService after a device reboot so
 * background notifications survive a power cycle. BOOT_COMPLETED is one of the
 * allowed entry points for starting a foreground service from the background.
 */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent?) {
        if (intent?.action == Intent.ACTION_BOOT_COMPLETED) {
            NotificationService.start(context)
        }
    }
}
