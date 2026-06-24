package io.gitlab.hongtao1207.nudge.protocol

import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okio.ByteString
import okio.ByteString.Companion.toByteString
import java.util.concurrent.TimeUnit

// The relay client — the Kotlin peer of Rust's transport::client over the WS path.
// Dials the rendezvous room over a wss WebSocket, runs the attach handshake, and
// seals/opens every frame with the pairing's E2E key so the relay only sees
// ciphertext. One WS Binary message carries exactly one sealed frame — the same
// one-frame-per-message rule the daemon's WsReader/WsWriter use (no newline framing).
class RelayClient(
    private val pairing: Pairing,
    private val listener: Listener,
    private val httpClient: OkHttpClient = defaultHttpClient,
) {
    // Callbacks fire on OkHttp's WebSocket thread; an Android caller marshals to the
    // main thread. Defaulted methods keep a listener that only cares about events terse.
    interface Listener {
        fun onAttached() {}
        fun onBusy() {}
        fun onEvent(seq: Long, event: ControllerEvent) {}
        fun onClosed(code: Int, reason: String) {}
        fun onFailure(error: Throwable) {}
    }

    // Highest seq seen so far — the resume cursor for a reconnect (Attach{after_seq}).
    @Volatile
    var lastSeq: Long? = null
        private set

    private var webSocket: WebSocket? = null

    // Open the connection and attach. afterSeq = null is a fresh attach (full replay);
    // pass lastSeq to resume after a drop and replay only the events that were missed.
    fun connect(afterSeq: Long? = null) {
        val request = Request.Builder().url(toHttpUrl(pairing.dialUrl())).build()
        webSocket = httpClient.newWebSocket(
            request,
            object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    sendFrame(webSocket, ClientFrame.Attach(afterSeq))
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

    // Send an application command (user message, model switch, permission answer, …).
    fun send(command: UiEvent) {
        webSocket?.let { sendFrame(it, ClientFrame.Command(command)) }
    }

    // Yield the session without ending it — the loop keeps running headless.
    fun detach() {
        webSocket?.let { sendFrame(it, ClientFrame.Detach) }
    }

    // Close the socket. The daemon treats a dropped socket as an implicit detach.
    fun close() {
        webSocket?.close(NORMAL_CLOSURE, "client closing")
    }

    private fun sendFrame(ws: WebSocket, frame: ClientFrame) {
        val json = WireJson.encodeToString(ClientFrame.serializer(), frame).encodeToByteArray()
        ws.send(pairing.cipher.seal(json).toByteString())
    }

    companion object {
        private const val NORMAL_CLOSURE = 1000

        // OkHttp wants an http/https URL and maps it to ws/wss internally; normalize so
        // a wss:// dial URL (as the pairing carries) works unchanged.
        private fun toHttpUrl(url: String): String = when {
            url.startsWith("wss://") -> "https://" + url.removePrefix("wss://")
            url.startsWith("ws://") -> "http://" + url.removePrefix("ws://")
            else -> url
        }

        // No read timeout — a WebSocket is long-lived and idle between events; the ping
        // interval keeps the connection (and the relay's NAT mapping) from timing out.
        val defaultHttpClient: OkHttpClient by lazy {
            OkHttpClient.Builder()
                .pingInterval(20, TimeUnit.SECONDS)
                .readTimeout(0, TimeUnit.MILLISECONDS)
                .build()
        }
    }
}
