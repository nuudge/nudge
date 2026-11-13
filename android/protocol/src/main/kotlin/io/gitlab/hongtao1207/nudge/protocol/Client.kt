package io.gitlab.hongtao1207.nudge.protocol

import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import java.util.concurrent.TimeUnit

// One WS Binary message carries exactly one sealed frame (no newline framing), matching
// the daemon's WsReader/WsWriter.
class RelayClient(
    private val pairing: Pairing,
    private val listener: Listener,
    private val who: ClientIdentity = ClientIdentity.human("phone"),
    private val httpClient: OkHttpClient = defaultHttpClient,
) {
    // Callbacks fire on OkHttp's WebSocket thread; Android callers must marshal to main.
    interface Listener {
        fun onAttached() {}
        fun onBusy() {}
        fun onEvent(seq: Long, event: ControllerEvent) {}
        fun onClosed(code: Int, reason: String) {}
        fun onFailure(error: Throwable) {}
    }

    // Resume cursor for a reconnect (Attach{after_seq}).
    @Volatile
    var lastSeq: Long? = null
        private set

    private var webSocket: WebSocket? = null

    // afterSeq = null is a fresh attach (full replay).
    fun connect(afterSeq: Long? = null) {
        val request = Request.Builder().url(toHttpUrl(pairing.clientDialUrl())).build()
        webSocket = httpClient.newWebSocket(
            request,
            object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    sendFrame(webSocket, ClientFrame.Attach(afterSeq, who))
                }

                override fun onMessage(webSocket: WebSocket, bytes: ByteString) {
                    val plaintext = pairing.cipher.open(bytes.toByteArray())
                    val frame = WireJson.decodeFromString(ServerFrame.serializer(), plaintext.decodeToString())
                    when (frame) {
                        ServerFrame.Attached -> listener.onAttached()
                        ServerFrame.Busy -> listener.onBusy()
                        is ServerFrame.Event -> {
                            lastSeq = frame.seq
                            listener.onEvent(frame.seq, frame.event)
                        }
                    }
                }

                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(NORMAL_CLOSURE, null)
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    listener.onClosed(code, reason)
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    listener.onFailure(t)
                }
            },
        )
    }

    fun send(command: UiEvent) {
        webSocket?.let { sendFrame(it, ClientFrame.Command(command)) }
    }

    // Yields the session without ending it; the loop keeps running headless.
    fun detach() {
        webSocket?.let { sendFrame(it, ClientFrame.Detach) }
    }

    // The daemon treats a dropped socket as an implicit detach.
    fun close() {
        webSocket?.close(NORMAL_CLOSURE, "client closing")
    }

    private fun sendFrame(ws: WebSocket, frame: ClientFrame) {
        val json = WireJson.encodeToString(ClientFrame.serializer(), frame).encodeToByteArray()
        ws.send(pairing.cipher.seal(json).toByteString())
    }

    companion object {
        private const val NORMAL_CLOSURE = 1000

        // OkHttp only accepts http/https URLs and maps them to ws/wss internally.
        private fun toHttpUrl(url: String): String = when {
            url.startsWith("wss://") -> "https://" + url.removePrefix("wss://")
            url.startsWith("ws://") -> "http://" + url.removePrefix("ws://")
            else -> url
        }

        // No read timeout: the socket idles between events; pings keep the relay's NAT mapping alive.
        val defaultHttpClient: OkHttpClient by lazy {
            OkHttpClient.Builder()
                .pingInterval(20, TimeUnit.SECONDS)
                .readTimeout(0, TimeUnit.MILLISECONDS)
                .build()
        }
    }
}
