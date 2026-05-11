package run.chatto.desktop

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
import org.json.JSONObject
import java.io.File
import java.util.concurrent.TimeUnit

/**
 * Foreground service that maintains a GraphQL WebSocket subscription to receive
 * notification events even when the app is backgrounded. Uses the same
 * graphql-transport-ws protocol as the web client.
 */
class NotificationService : Service() {

    companion object {
        private const val TAG = "ChattoNotifService"
        private const val CHANNEL_ID = "chatto_foreground"
        private const val NOTIF_CHANNEL_ID = "chatto_messages"
        private const val FOREGROUND_NOTIF_ID = 1
        private const val DEFAULT_SERVER_URL = "https://chat.chatto.run"
        private const val MAX_RECONNECT_DELAY_MS = 60_000L

        /** Current room the user is viewing — suppress notifications for this room */
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
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setContentIntent(pending)
            .setOngoing(true)
            .build()
    }

    // --- Server URL & Auth ---

    private fun getServerUrl(): String {
        // Try Tauri's store file (config.json) in several possible locations
        val candidates = listOf(
            File(filesDir, "config.json"),
            File(filesDir, ".config.json"),
            File(File(filesDir, "app_data"), "config.json"),
        )
        for (f in candidates) {
            try {
                if (f.exists()) {
                    val json = JSONObject(f.readText())
                    val url = json.optString("server_url", "")
                    if (url.isNotBlank()) {
                        Log.d(TAG, "Read server URL from ${f.path}: $url")
                        return url
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "Failed to read ${f.path}: ${e.message}")
            }
        }
        Log.d(TAG, "Using default server URL: $DEFAULT_SERVER_URL")
        return DEFAULT_SERVER_URL
    }

    private fun getCookies(serverUrl: String): String? {
        return try {
            val cm = CookieManager.getInstance()
            // The WebView may not have flushed cookies to the persistent
            // store yet — flush before reading so freshly-set session cookies
            // are visible to this service.
            cm.flush()
            val cookies = cm.getCookie(serverUrl)
            if (cookies.isNullOrBlank()) {
                Log.w(TAG, "CookieManager returned no cookies for $serverUrl")
            } else {
                // Don't log cookie values; only names so we know what's available.
                val names = cookies.split(';').mapNotNull { it.substringBefore('=', "").trim().takeIf { n -> n.isNotEmpty() } }
                Log.d(TAG, "CookieManager has cookies: $names")
            }
            cookies
        } catch (e: Exception) {
            Log.w(TAG, "Failed to read cookies: ${e.message}")
            null
        }
    }

    // --- WebSocket Connection ---

    private fun connectIfNeeded() {
        if (isConnected) return

        val serverUrl = getServerUrl()
        val cookies = getCookies(serverUrl)
        if (cookies.isNullOrBlank()) {
            Log.w(TAG, "No cookies available — user may not be logged in. Retrying later.")
            scheduleReconnect()
            return
        }

        val wsUrl = serverUrl
            .replace("https://", "wss://")
            .replace("http://", "ws://") + "/api/graphql"

        Log.d(TAG, "Connecting to $wsUrl")

        client = OkHttpClient.Builder()
            .readTimeout(0, TimeUnit.MILLISECONDS)
            .pingInterval(30, TimeUnit.SECONDS)
            .build()

        val request = Request.Builder()
            .url(wsUrl)
            .header("Cookie", cookies)
            .header("Sec-WebSocket-Protocol", "graphql-transport-ws")
            .build()

        webSocket = client?.newWebSocket(request, WsListener())
    }

    private fun scheduleReconnect() {
        val delay = minOf(
            (1000L * (1 shl minOf(reconnectAttempt, 6))),
            MAX_RECONNECT_DELAY_MS
        )
        reconnectAttempt++
        Log.d(TAG, "Reconnecting in ${delay}ms (attempt $reconnectAttempt)")
        android.os.Handler(mainLooper).postDelayed({ connectIfNeeded() }, delay)
    }

    private inner class WsListener : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            Log.d(TAG, "WebSocket opened")
            isConnected = true
            reconnectAttempt = 0
            // graphql-transport-ws: connection_init
            webSocket.send("""{"type":"connection_init"}""")
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            try {
                handleWsMessage(webSocket, JSONObject(text))
            } catch (e: Exception) {
                Log.w(TAG, "Failed to handle message: ${e.message}")
            }
        }

        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
            Log.d(TAG, "WebSocket closed: $code $reason")
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

    private fun handleWsMessage(ws: WebSocket, msg: JSONObject) {
        when (msg.optString("type")) {
            "connection_ack" -> {
                Log.d(TAG, "Connection acknowledged, subscribing to events")
                val sub = JSONObject().apply {
                    put("id", "1")
                    put("type", "subscribe")
                    put("payload", JSONObject().apply {
                        put("query", """
                            subscription {
                                myInstanceEvents {
                                    event {
                                        __typename
                                        ... on NotificationCreatedEvent { roomId eventId }
                                    }
                                }
                            }
                        """.trimIndent())
                    })
                }
                ws.send(sub.toString())
            }
            "next" -> {
                val event = msg.optJSONObject("payload")
                    ?.optJSONObject("data")
                    ?.optJSONObject("myInstanceEvents")
                    ?.optJSONObject("event")
                    ?: return

                val typeName = event.optString("__typename")
                Log.d(TAG, "Event: $typeName ${event.toString().take(200)}")
                if (typeName == "NotificationCreatedEvent") {
                    val roomId = event.optString("roomId", "")
                    val eventId = event.optString("eventId", "").takeIf { it.isNotBlank() }
                    if (roomId == activeRoomId) return
                    if (roomId.isNotBlank() && shouldFire(roomId)) {
                        fetchRoomEventAndNotify(roomId, eventId)
                    } else {
                        Log.d(TAG, "Skipped: roomId=$roomId activeRoom=$activeRoomId shouldFire=${roomId.isNotBlank()}")
                    }
                }
            }
            "error" -> {
                Log.w(TAG, "Subscription error: ${msg.optJSONArray("payload")}")
            }
        }
    }

    // --- Notification Deduplication ---

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

    // --- Fetch message body via GraphQL HTTP ---

    private fun fetchRoomEventAndNotify(roomId: String, eventId: String?) {
        val serverUrl = getServerUrl()
        val cookies = getCookies(serverUrl) ?: return

        // Prefer fetching the exact event (we get eventId from NotificationCreatedEvent
        // since spaces were phased out). Fall back to the room's latest event.
        val gql = if (eventId != null) {
            "query(\$r:ID!,\$e:ID!){roomEventByEventId(roomId:\$r,eventId:\$e){actor{displayName}event{__typename...on MessagePostedEvent{body}}} room(roomId:\$r){name}}"
        } else {
            "query(\$r:ID!){roomEvents(roomId:\$r,limit:1){actor{displayName}event{__typename...on MessagePostedEvent{body}}} room(roomId:\$r){name}}"
        }
        val query = JSONObject().apply {
            put("query", gql)
            put("variables", JSONObject().apply {
                put("r", roomId)
                if (eventId != null) put("e", eventId)
            })
        }

        val body = query.toString().toRequestBody("application/json".toMediaTypeOrNull())

        val request = Request.Builder()
            .url("$serverUrl/api/graphql")
            .header("Cookie", cookies)
            .post(body)
            .build()

        val chatUrl = "$serverUrl/chat/$roomId"

        client?.newCall(request)?.enqueue(object : Callback {
            override fun onFailure(call: Call, e: java.io.IOException) {
                Log.w(TAG, "Failed to fetch room event: ${e.message}")
                showMessageNotification("Chatto", "New message (network error)", null, chatUrl)
            }

            override fun onResponse(call: Call, response: Response) {
                try {
                    val rawBody = response.body?.string() ?: "{}"
                    val code = response.code
                    Log.d(TAG, "GraphQL response (HTTP $code, ${rawBody.length} bytes): ${rawBody.take(800)}")

                    if (code != 200) {
                        Log.w(TAG, "GraphQL non-200 response: $code")
                        showMessageNotification("Chatto", "New message (HTTP $code)", null, chatUrl)
                        return
                    }

                    val json = JSONObject(rawBody)
                    val errors = json.optJSONArray("errors")
                    if (errors != null && errors.length() > 0) {
                        Log.w(TAG, "GraphQL errors: $errors")
                        showMessageNotification("Chatto", "New message (gql error)", null, chatUrl)
                        return
                    }

                    val data = json.optJSONObject("data")
                    val roomName = data?.optJSONObject("room")?.optString("name")?.takeIf { it.isNotBlank() }

                    // Either single event (roomEventByEventId) or array of events (roomEvents).
                    val ev = data?.optJSONObject("roomEventByEventId")
                        ?: data?.optJSONArray("roomEvents")?.takeIf { it.length() > 0 }?.getJSONObject(0)

                    if (ev != null) {
                        val actor = ev.optJSONObject("actor")?.optString("displayName") ?: "Chatto"
                        val eventNode = ev.optJSONObject("event")
                        val msgBody = eventNode?.let { node ->
                            listOf("body", "text", "content", "message").firstNotNullOfOrNull { f ->
                                node.optString(f, "").takeIf { it.isNotBlank() }
                            }
                        }
                        if (!msgBody.isNullOrBlank()) {
                            showMessageNotification(actor, msgBody, roomName, chatUrl)
                            return
                        } else {
                            Log.w(TAG, "Event had no recognised body field. Type: ${eventNode?.optString("__typename")} keys: ${eventNode?.keys()?.asSequence()?.toList()}")
                        }
                    } else {
                        Log.w(TAG, "GraphQL returned no event for roomId=$roomId eventId=$eventId")
                    }
                    showMessageNotification("Chatto", "New message (empty body)", roomName, chatUrl)
                } catch (e: Exception) {
                    Log.w(TAG, "Failed to parse room event: ${e.message}", e)
                    showMessageNotification("Chatto", "New message (parse error)", null, chatUrl)
                }
            }
        })
    }

    // --- Show Notification ---

    private var notifCounter = 100

    private fun showMessageNotification(
        title: String,
        body: String,
        roomName: String?,
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
            .setSmallIcon(android.R.drawable.ic_dialog_email)
            .setContentIntent(pending)
            .setAutoCancel(true)

        if (!roomName.isNullOrBlank()) {
            builder.setSubText("#$roomName")
        }

        val mgr = getSystemService(NotificationManager::class.java)
        mgr.notify(notifCounter++, builder.build())
    }
}
