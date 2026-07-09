package run.chatto.desktop

import android.app.AlarmManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import android.util.Log
import android.webkit.CookieManager
import okhttp3.*
import okhttp3.MediaType.Companion.toMediaTypeOrNull
import okhttp3.RequestBody.Companion.toRequestBody
import okio.ByteString
import okio.ByteString.Companion.toByteString
import org.json.JSONObject
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Foreground service that keeps a binary-protobuf realtime WebSocket open
 * (wss://<host>/api/realtime) so notifications arrive while the app is
 * backgrounded. Decodes RealtimeServerFrame.event -> RealtimeEventEnvelope ->
 * RealtimeNotificationCreatedEvent with a tiny hand-rolled protobuf reader, then
 * hydrates the message body over ConnectRPC JSON.
 */
class NotificationService : Service() {

    companion object {
        private const val TAG = "ChattoNotifService"
        private const val CHANNEL_ID = "chatto_foreground"
        private const val NOTIF_CHANNEL_ID = "chatto_messages"
        private const val FOREGROUND_NOTIF_ID = 1
        private const val DEFAULT_SERVER_URL = "https://chat.chatto.run"
        private const val MAX_RECONNECT_DELAY_MS = 60_000L

        // Static client frames (see REALTIME_SPEC.md). Both are trivially small,
        // so we ship the exact bytes rather than a general encoder:
        //   RealtimeClientFrame.hello{ protocol_version = 1 }
        //     field 1 (hello, LEN) tag=0x0A len=0x02 -> [ field 1 (uint32, VARINT) tag=0x08 value=0x01 ]
        private val FRAME_HELLO = byteArrayOf(0x0A, 0x02, 0x08, 0x01)
        //   RealtimeClientFrame.subscribe_events{} (empty message)
        //     field 2 (subscribe_events, LEN) tag=0x12 len=0x00
        private val FRAME_SUBSCRIBE = byteArrayOf(0x12, 0x00)

        /** Current room the user is viewing, suppress notifications for this room */
        @Volatile
        var activeRoomId: String? = null

        fun start(context: Context) {
            val intent = Intent(context, NotificationService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }
    }

    private var client: OkHttpClient? = null
    private var webSocket: WebSocket? = null
    private var reconnectAttempt = 0
    private var isConnected = false
    private var handshakeDone = false
    private val recentNotifKeys = mutableMapOf<String, Long>()

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        createNotificationChannels()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        try {
            startForeground(FOREGROUND_NOTIF_ID, buildForegroundNotification())
        } catch (e: Exception) {
            Log.w(TAG, "Could not start foreground: ${e.message}")
        }
        connectIfNeeded()
        return START_STICKY
    }

    override fun onDestroy() {
        webSocket?.close(1000, "Service stopped")
        client?.dispatcher?.executorService?.shutdown()
        super.onDestroy()
    }

    /**
     * Swiping the app away from Recents kills the task; reschedule a restart so
     * the background WebSocket comes back. START_STICKY alone isn't reliable
     * across OEMs, so we also arm an alarm.
     */
    override fun onTaskRemoved(rootIntent: Intent?) {
        val restart = Intent(applicationContext, NotificationService::class.java)
        val pending = PendingIntent.getService(
            this, 1, restart,
            PendingIntent.FLAG_ONE_SHOT or PendingIntent.FLAG_IMMUTABLE
        )
        val am = getSystemService(Context.ALARM_SERVICE) as AlarmManager
        am.set(AlarmManager.RTC, System.currentTimeMillis() + 2000, pending)
        super.onTaskRemoved(rootIntent)
    }

    private fun createNotificationChannels() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val mgr = getSystemService(NotificationManager::class.java)

            // Low-priority channel for the persistent foreground notification
            val foreground = NotificationChannel(
                CHANNEL_ID, "Background sync",
                NotificationManager.IMPORTANCE_LOW
            ).apply {
                description = "Keeps Chatto connected for notifications"
                setShowBadge(false)
            }
            mgr.createNotificationChannel(foreground)

            // Default-priority channel for actual message notifications
            val messages = NotificationChannel(
                NOTIF_CHANNEL_ID, "Messages",
                NotificationManager.IMPORTANCE_DEFAULT
            ).apply {
                description = "Chat messages and mentions"
            }
            mgr.createNotificationChannel(messages)
        }
    }

    private fun buildForegroundNotification(): Notification {
        val intent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP
        }
        val pending = PendingIntent.getActivity(
            this, 0, intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("Chatto")
            .setContentText("Connected for notifications")
            .setSmallIcon(R.drawable.ic_stat_chatto)
            .setContentIntent(pending)
            .setOngoing(true)
            .build()
    }

    // Config store (Tauri's config.json)

    /** First readable Tauri store file, or null. */
    private fun readConfig(): JSONObject? {
        val candidates = listOf(
            File(filesDir, "config.json"),
            File(filesDir, ".config.json"),
            File(File(filesDir, "app_data"), "config.json"),
        )
        for (f in candidates) {
            try {
                if (f.exists()) return JSONObject(f.readText())
            } catch (e: Exception) {
                if (BuildConfig.DEBUG) Log.w(TAG, "Failed to read ${f.path}: ${e.message}")
            }
        }
        return null
    }

    private fun getServerUrl(): String {
        readConfig()?.optString("server_url", "")?.takeIf { it.isNotBlank() }?.let {
            if (BuildConfig.DEBUG) Log.d(TAG, "Server URL from config: $it")
            return it
        }
        if (BuildConfig.DEBUG) Log.d(TAG, "Using default server URL: $DEFAULT_SERVER_URL")
        return DEFAULT_SERVER_URL
    }

    /** Honor the in-app notification toggle (default true), matching the desktop Rust path. */
    private fun getNotificationsEnabled(): Boolean {
        return readConfig()?.let {
            if (it.has("notifications_enabled")) it.optBoolean("notifications_enabled", true) else true
        } ?: true
    }

    private fun getCookies(serverUrl: String): String? {
        return try {
            val cm = CookieManager.getInstance()
            // The WebView may not have flushed cookies to the persistent store
            // yet, flush before reading so freshly-set session cookies are
            // visible to this service.
            cm.flush()
            val cookies = cm.getCookie(serverUrl)
            if (cookies.isNullOrBlank()) {
                Log.w(TAG, "CookieManager returned no cookies for the server")
            } else if (BuildConfig.DEBUG) {
                // Never log cookie values; only names so we know what's available.
                val names = cookies.split(';')
                    .mapNotNull { it.substringBefore('=', "").trim().takeIf { n -> n.isNotEmpty() } }
                Log.d(TAG, "CookieManager has cookies: $names")
            }
            cookies
        } catch (e: Exception) {
            Log.w(TAG, "Failed to read cookies: ${e.message}")
            null
        }
    }

    // WebSocket Connection

    private fun connectIfNeeded() {
        if (isConnected) return

        val serverUrl = getServerUrl()
        val cookies = getCookies(serverUrl)
        if (cookies.isNullOrBlank()) {
            Log.w(TAG, "No cookies available, user may not be logged in. Retrying later.")
            scheduleReconnect()
            return
        }

        val wsUrl = serverUrl
            .replace("https://", "wss://")
            .replace("http://", "ws://") + "/api/realtime"

        if (BuildConfig.DEBUG) Log.d(TAG, "Connecting to $wsUrl")

        client = OkHttpClient.Builder()
            .readTimeout(0, TimeUnit.MILLISECONDS)
            .pingInterval(30, TimeUnit.SECONDS)
            .build()

        handshakeDone = false
        val request = Request.Builder()
            .url(wsUrl)
            .header("Cookie", cookies)
            .build()

        webSocket = client?.newWebSocket(request, WsListener())
    }

    private fun scheduleReconnect() {
        val delay = minOf(
            (1000L * (1 shl minOf(reconnectAttempt, 6))),
            MAX_RECONNECT_DELAY_MS
        )
        reconnectAttempt++
        if (BuildConfig.DEBUG) Log.d(TAG, "Reconnecting in ${delay}ms (attempt $reconnectAttempt)")
        android.os.Handler(mainLooper).postDelayed({ connectIfNeeded() }, delay)
    }

    private inner class WsListener : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            if (BuildConfig.DEBUG) Log.d(TAG, "WebSocket opened")
            isConnected = true
            reconnectAttempt = 0
            handshakeDone = false
            // Step 1 of the handshake: send RealtimeClientFrame.hello.
            webSocket.send(FRAME_HELLO.toByteString())
        }

        override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
            try {
                handleServerFrame(webSocket, bytes.toByteArray())
            } catch (e: Exception) {
                if (BuildConfig.DEBUG) Log.w(TAG, "Failed to handle frame: ${e.message}")
            }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            // The realtime protocol is binary; a text frame is unexpected. Ignore.
            if (BuildConfig.DEBUG) Log.d(TAG, "Ignoring unexpected text frame")
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            if (BuildConfig.DEBUG) Log.d(TAG, "WebSocket closed: $code")
            isConnected = false
            scheduleReconnect()
        }

        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
            Log.w(TAG, "WebSocket failure: ${t.message}")
            isConnected = false
            this@NotificationService.webSocket = null
            scheduleReconnect()
        }
    }

    // Minimal protobuf reader

    /**
     * Walks a protobuf message field by field. Only the two wire types we need
     * (VARINT and LEN) carry values we read; I64/I32 are skipped. This is enough
     * to pick out the handful of fields the spec calls for without a codegen
     * pipeline or runtime library.
     */
    private class ProtoReader(private val buf: ByteArray) {
        private var pos = 0
        private val end = buf.size

        fun hasMore(): Boolean = pos < end

        private fun readVarint(): Long {
            var result = 0L
            var shift = 0
            while (pos < end && shift < 64) {
                val b = buf[pos++].toInt() and 0xFF
                result = result or ((b and 0x7F).toLong() shl shift)
                if (b and 0x80 == 0) return result
                shift += 7
            }
            return result
        }

        var wireType: Int = 0
            private set

        /**
         * Read the next tag varint: returns the field number and exposes the
         * wire type via [wireType]. Caller then reads or skips the value.
         */
        fun nextField(): Int {
            val tag = readVarint()
            wireType = (tag and 0x7).toInt()
            return (tag ushr 3).toInt()
        }

        fun readVarintValue(): Long = readVarint()

        fun readLenBytes(): ByteArray {
            val len = readVarint().toInt().coerceIn(0, end - pos)
            val start = pos
            pos += len
            return buf.copyOfRange(start, start + len)
        }

        fun readString(): String = String(readLenBytes(), Charsets.UTF_8)

        /** Consume the current field's value without interpreting it. */
        fun skipValue() {
            when (wireType) {
                0 -> readVarint()
                2 -> { val len = readVarint().toInt().coerceIn(0, end - pos); pos += len }
                1 -> pos = minOf(end, pos + 8)
                5 -> pos = minOf(end, pos + 4)
                else -> pos = end // unknown wire type, bail out of this message
            }
        }
    }

    // Frame decoding

    /** Top-level RealtimeServerFrame oneof. */
    private fun handleServerFrame(ws: WebSocket, data: ByteArray) {
        val r = ProtoReader(data)
        var hasHello = false
        var eventBytes: ByteArray? = null
        while (r.hasMore()) {
            val field = r.nextField()
            when {
                field == 1 && r.wireType == 2 -> { hasHello = true; r.readLenBytes() } // hello
                field == 3 && r.wireType == 2 -> eventBytes = r.readLenBytes()          // event
                else -> r.skipValue() // heartbeat / pong / error / close / subscribed / unknown
            }
        }

        if (hasHello && !handshakeDone) {
            // Step 2 of the handshake: subscribe to events.
            handshakeDone = true
            ws.send(FRAME_SUBSCRIBE.toByteString())
            if (BuildConfig.DEBUG) Log.d(TAG, "Server hello received, subscribed to events")
        }

        eventBytes?.let { decodeEnvelope(it) }
    }

    /** RealtimeEventEnvelope, we only care about field 60 (notification_created). */
    private fun decodeEnvelope(data: ByteArray) {
        val r = ProtoReader(data)
        var envelopeId: String? = null
        var notifBytes: ByteArray? = null
        while (r.hasMore()) {
            val field = r.nextField()
            when {
                field == 1 && r.wireType == 2 -> envelopeId = r.readString()       // envelope id
                field == 60 && r.wireType == 2 -> notifBytes = r.readLenBytes()    // notification_created
                else -> r.skipValue() // other event kinds (message_posted, mention, ...), ignore
            }
        }
        val nb = notifBytes ?: return // key ONLY on notification_created (avoids double-firing)
        if (BuildConfig.DEBUG) Log.d(TAG, "notification_created envelope=$envelopeId")
        decodeNotificationCreated(nb)
    }

    /** RealtimeNotificationCreatedEvent -> decision logic + hydration. */
    private fun decodeNotificationCreated(data: ByteArray) {
        val r = ProtoReader(data)
        var notificationId: String? = null
        var roomId: String? = null
        var eventId: String? = null
        var silent = false
        while (r.hasMore()) {
            val field = r.nextField()
            when {
                field == 1 && r.wireType == 2 -> notificationId = r.readString()
                field == 2 && r.wireType == 2 -> roomId = r.readString()
                field == 3 && r.wireType == 2 -> eventId = r.readString()
                field == 5 && r.wireType == 0 -> silent = r.readVarintValue() != 0L
                else -> r.skipValue()
            }
        }

        if (silent) return                                    // no alert requested
        val room = roomId?.takeIf { it.isNotBlank() } ?: return // nothing to open
        if (room == activeRoomId) return                      // user already viewing it

        val ev = eventId?.takeIf { it.isNotBlank() }
        // Dedup by event_id (distinct per message) falling back to notification_id.
        val dedupKey = ev ?: notificationId?.takeIf { it.isNotBlank() } ?: room
        if (!shouldFire(dedupKey)) return

        fetchRoomEventAndNotify(room, ev)
    }

    // Notification Deduplication

    private fun shouldFire(key: String): Boolean {
        val now = System.currentTimeMillis()
        val last = recentNotifKeys[key]
        if (last != null && now - last < 3000) return false
        recentNotifKeys[key] = now
        // Cleanup old entries
        if (recentNotifKeys.size > 100) {
            recentNotifKeys.entries.removeAll { now - it.value > 10_000 }
        }
        return true
    }

    // Hydrate message via ConnectRPC JSON

    private fun buildDeepLink(serverUrl: String, roomId: String, eventId: String?): String {
        // The "-" is the literal home-server segment (HOME_SEGMENT), required.
        val base = "$serverUrl/chat/-/$roomId"
        return if (eventId != null) "$base?highlight=$eventId" else base
    }

    private fun fetchRoomEventAndNotify(roomId: String, eventId: String?) {
        // Respect the in-app toggle before doing any work or alerting.
        if (!getNotificationsEnabled()) {
            if (BuildConfig.DEBUG) Log.d(TAG, "Notifications disabled in config, skipping")
            return
        }

        val serverUrl = getServerUrl()
        val cookies = getCookies(serverUrl) ?: return
        val deepLink = buildDeepLink(serverUrl, roomId, eventId)

        // Prefer the exact event; fall back to the room's latest message.
        val method: String
        val payload = JSONObject()
        if (eventId != null) {
            method = "GetRoomEventsAround"
            payload.put("roomId", roomId)
            payload.put("eventId", eventId)
            payload.put("limit", 1)
        } else {
            method = "GetRoomEvents"
            payload.put("roomId", roomId)
            payload.put("limit", 1)
        }

        val url = "$serverUrl/api/connect/chatto.api.v1.RoomService/$method"
        val body = payload.toString().toRequestBody("application/json".toMediaTypeOrNull())
        val request = Request.Builder()
            .url(url)
            .header("Cookie", cookies)
            .header("Content-Type", "application/json")
            .post(body)
            .build()

        client?.newCall(request)?.enqueue(object : Callback {
            override fun onFailure(call: Call, e: java.io.IOException) {
                if (BuildConfig.DEBUG) Log.w(TAG, "Hydrate request failed: ${e.message}")
                showMessageNotification("Chatto", "New message", deepLink)
            }

            override fun onResponse(call: Call, response: Response) {
                try {
                    val raw = response.body?.string() ?: "{}"
                    val code = response.code
                    if (BuildConfig.DEBUG) Log.d(TAG, "Hydrate HTTP $code (${raw.length} bytes)")
                    if (code != 200) {
                        showMessageNotification("Chatto", "New message", deepLink)
                        return
                    }
                    parseAndNotify(JSONObject(raw), eventId, deepLink)
                } catch (e: Exception) {
                    if (BuildConfig.DEBUG) Log.w(TAG, "Failed to parse hydrate response: ${e.message}")
                    showMessageNotification("Chatto", "New message", deepLink)
                }
            }
        })
    }

    private fun parseAndNotify(json: JSONObject, eventId: String?, deepLink: String) {
        val page = json.optJSONObject("page")
        val events = page?.optJSONArray("events")
        if (page == null || events == null || events.length() == 0) {
            showMessageNotification("Chatto", "New message", deepLink)
            return
        }

        fun messagePostedOf(e: JSONObject?): JSONObject? = e?.optJSONObject("messagePosted")

        // Choose the anchor event: exact id match first, then targetIndex,
        // then the last event that is actually a posted message.
        var anchor: JSONObject? = null
        if (eventId != null) {
            for (i in 0 until events.length()) {
                val e = events.optJSONObject(i)
                if (e?.optString("id") == eventId && messagePostedOf(e) != null) {
                    anchor = e
                    break
                }
            }
            if (anchor == null) {
                val ti = json.optInt("targetIndex", -1)
                val cand = if (ti in 0 until events.length()) events.optJSONObject(ti) else null
                if (cand != null && messagePostedOf(cand) != null) anchor = cand
            }
        }
        if (anchor == null) {
            for (i in events.length() - 1 downTo 0) {
                val e = events.optJSONObject(i)
                if (messagePostedOf(e) != null) {
                    anchor = e
                    break
                }
            }
        }

        val messagePosted = messagePostedOf(anchor)
        if (messagePosted == null) {
            // Not a chat message (join/leave/room event), nothing to show.
            if (BuildConfig.DEBUG) Log.d(TAG, "No messagePosted in anchor, suppressing")
            return
        }
        val message = messagePosted.optJSONObject("message")
        val msgBody = message?.optString("body", "")?.takeIf { it.isNotBlank() }
        if (msgBody == null) return // not a chat message, nothing to show

        val actorId = message.optString("actorId", "").takeIf { it.isNotBlank() }
            ?: anchor?.optString("actorId", "")?.takeIf { it.isNotBlank() }
        val displayName = actorId?.let {
            page.optJSONObject("includes")
                ?.optJSONObject("users")
                ?.optJSONObject(it)
                ?.optString("displayName", "")
        }?.takeIf { it.isNotBlank() }

        showMessageNotification(displayName ?: "Chatto", msgBody, deepLink)
    }

    // Show Notification

    private var notifCounter = 100

    private fun showMessageNotification(
        title: String,
        body: String,
        navigateUrl: String?
    ) {
        val intent = Intent(this, MainActivity::class.java).apply {
            flags = Intent.FLAG_ACTIVITY_SINGLE_TOP
            if (navigateUrl != null) {
                putExtra("navigate_url", navigateUrl)
            }
        }
        val pending = PendingIntent.getActivity(
            this, notifCounter, intent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )

        val builder = Notification.Builder(this, NOTIF_CHANNEL_ID)
            .setContentTitle(title)
            .setContentText(body)
            .setSmallIcon(R.drawable.ic_stat_chatto)
            .setContentIntent(pending)
            .setAutoCancel(true)

        val mgr = getSystemService(NotificationManager::class.java)
        mgr.notify(notifCounter++, builder.build())
    }
}
