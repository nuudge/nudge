package io.gitlab.hongtao1207.nudge.app

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.gitlab.hongtao1207.nudge.protocol.ControllerEvent
import io.gitlab.hongtao1207.nudge.protocol.Pairing
import io.gitlab.hongtao1207.nudge.protocol.RelayClient
import io.gitlab.hongtao1207.nudge.protocol.UiEvent
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update

// Disconnected = unbound (no saved pairing → pairing screen). Detached = bound to a
// daemon but paused, awaiting a manual Reattach (saved pairing kept).
enum class Connection { Disconnected, Connecting, Attached, Busy, Error, Detached }

enum class Role { User, Assistant, Thinking, ToolCall, ToolResult, System }

// `text` is the primary line (assistant markdown, user message, tool name, …); `body`
// is optional secondary content rendered collapsibly (a tool call's arguments, a tool
// result's output). `isError` tints tool results that failed.
data class ChatLine(
    val id: Long,
    val role: Role,
    val text: String,
    val body: String? = null,
    val isError: Boolean = false,
)

// The tool-call permission the agent is currently blocked on, awaiting our answer.
data class PendingPermission(val toolUseId: String, val toolName: String, val summary: String)

// Static session context for the header, from the daemon's SessionInfo event (replayed
// first on attach). gitBranch is null when the cwd isn't a git repo. sessionName is the
// human label (null until renamed); the header shows it in place of the uuid.
data class SessionContext(
    val model: String,
    val cwd: String,
    val gitBranch: String?,
    val sessionId: String,
    val sessionName: String?,
)

data class ChatUiState(
    val connection: Connection = Connection.Disconnected,
    val status: String = "Not connected",
    val lines: List<ChatLine> = emptyList(),
    val turnInFlight: Boolean = false,
    val pendingPermission: PendingPermission? = null,
    val sessionInfo: SessionContext? = null,
)

// Bridges the pure-JVM RelayClient to Compose. RelayClient's listener callbacks fire on
// OkHttp's WebSocket thread; updating a MutableStateFlow from there is thread-safe and
// Compose collects the flow on the main thread, so no explicit handler marshalling is
// needed. User and assistant bubbles are both rendered from the daemon's event stream
// (the daemon echoes UserMessage back) — one source of truth, so reconnect-replay
// reproduces the transcript correctly without client-side bookkeeping.
class ChatViewModel(app: Application) : AndroidViewModel(app) {
    private val _state = MutableStateFlow(ChatUiState())
    val state: StateFlow<ChatUiState> = _state.asStateFlow()

    // App-private store for the last working pairing code, so backgrounding or being
    // killed doesn't force a re-scan. NOTE: the code embeds the E2E key — this persists a
    // secret to disk. It's app-private and allowBackup=false (manifest), acceptable for a
    // personal tool; EncryptedSharedPreferences/Keystore would harden it further.
    private val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    private var client: RelayClient? = null
    private var pairing: Pairing? = null
    private var nextId = 0L

    // Watch the default network so a wifi↔cellular handoff redials immediately instead of
    // waiting ~20-40s for OkHttp's ping to notice the stranded socket. Callbacks arrive off
    // the main thread, so each hops back to Main (viewModelScope) before touching state.
    private val connectivityManager = app.getSystemService(ConnectivityManager::class.java)
    private var lastNetwork: Network? = null
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            viewModelScope.launch { onNetworkAvailable(network) }
        }
    }

    // Bumped on every connect and detach. A connection's listener is tagged with the
    // epoch it was born in and ignores its own callbacks once the epoch moves on, so the
    // socket we just left can't clobber the UI with a late teardown event. Read on
    // OkHttp's thread, written on the main thread — @Volatile keeps it visible.
    @Volatile
    private var generation = 0

    // Auto-reconnect state. wantConnected is true between a successful connect() and an
    // intentional detach; a drop while it's set triggers a backoff redial that resumes
    // from lastSeq (replaying only missed events). Read on OkHttp's thread → @Volatile.
    @Volatile
    private var wantConnected = false
    private var reconnectAttempts = 0
    private var reconnectJob: Job? = null

    init {
        connectivityManager?.registerDefaultNetworkCallback(networkCallback)
        // Cold launch into the detached state: reflect "bound but paused" so the UI shows
        // Reattach (not the scanner) and resume() leaves it alone until the user taps.
        if (prefs.getString(KEY_CODE, null) != null && prefs.getBoolean(KEY_PAUSED, false)) {
            _state.update { it.copy(connection = Connection.Detached, status = "Detached") }
        }
    }

    fun connect(code: String) {
        reconnectJob?.cancel()
        // Fresh transcript: a plain connect does a full replay (after_seq = null), so
        // starting empty avoids doubling earlier lines. (Reconnects below instead resume
        // from lastSeq and keep the transcript.)
        _state.update {
            it.copy(
                connection = Connection.Connecting,
                status = "Connecting…",
                lines = emptyList(),
                pendingPermission = null,
                sessionInfo = null,
            )
        }
        val decoded = try {
            Pairing.decode(code, AndroidSodium.secretBox)
        } catch (e: Exception) {
            _state.update { it.copy(connection = Connection.Error, status = "Bad pairing code: ${e.message}") }
            return
        }
        pairing = decoded
        wantConnected = true
        reconnectAttempts = 0
        prefs.edit().putString(KEY_CODE, code).remove(KEY_PAUSED).apply() // bind + un-pause
        dial(afterSeq = null) // fresh attach → full replay
    }

    // Open a connection (or reconnection) on a fresh epoch, so any prior socket's late
    // callbacks (e.g. a stranded one finally erroring after a handoff) are ignored and
    // can't bounce the healthy new connection. afterSeq = null replays everything; a
    // cursor resumes and replays only the events missed during the gap.
    private fun dial(afterSeq: Long?) {
        val p = pairing ?: return
        client?.close()
        generation++
        client = RelayClient(p, listener(generation)).also { it.connect(afterSeq) }
    }

    // Called when the app returns to the foreground (or launches). If we're bound and
    // active, re-attach from the saved pairing (stops a "switch away" or app-kill from
    // forcing a re-scan). If we're bound but *paused* (the user detached), stay put and
    // wait for a manual Reattach. Unbound → nothing (the pairing screen shows).
    fun resume() {
        val current = _state.value.connection
        if (current == Connection.Attached || current == Connection.Connecting) return
        val code = prefs.getString(KEY_CODE, null) ?: return
        if (prefs.getBoolean(KEY_PAUSED, false)) {
            _state.update { it.copy(connection = Connection.Detached, status = "Detached") }
        } else {
            connect(code)
        }
    }

    // Temporarily leave but stay bound to this daemon: stop the reconnect loop, mark the
    // pairing paused (kept on disk), and wait for a manual Reattach. The agent keeps
    // running headless — at the wire level this is the same Detach the daemon always sees.
    fun detach() {
        wantConnected = false
        reconnectJob?.cancel()
        prefs.edit().putBoolean(KEY_PAUSED, true).apply() // keep the code; mark paused
        generation++
        client?.detach()
        client?.close()
        client = null
        _state.update {
            it.copy(
                connection = Connection.Detached,
                status = "Detached",
                turnInFlight = false,
                pendingPermission = null,
            )
        }
    }

    // Reattach to the SAME daemon from the saved pairing — the only target a detach allows.
    fun reattach() {
        prefs.getString(KEY_CODE, null)?.let { connect(it) }
    }

    // Unbind: forget the pairing entirely (switching daemons, or done) and return to the
    // pairing screen, where any daemon can be scanned. The agent still runs headless on
    // the host — a controller can't end the session; only stopping the daemon process can.
    fun disconnect() {
        wantConnected = false
        reconnectJob?.cancel()
        prefs.edit().remove(KEY_CODE).remove(KEY_PAUSED).apply() // forget the binding
        generation++
        client?.detach()
        client?.close()
        client = null
        // Unbinding returns to the pairing screen; clear the transcript so a new pairing
        // starts fresh and stale history doesn't sit behind the scanner. (Detach keeps the
        // transcript — it's the same session, just paused.)
        _state.update {
            it.copy(
                connection = Connection.Disconnected,
                status = "Disconnected",
                lines = emptyList(),
                turnInFlight = false,
                pendingPermission = null,
                sessionInfo = null,
            )
        }
    }

    fun send(text: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty() || client == null) return
        _state.update { it.copy(turnInFlight = true) }
        client?.send(UiEvent.UserMessage(trimmed))
    }

    // Answer the tool-call permission the agent is blocked on. The daemon then streams a
    // PermissionResolved (and, if allowed, the tool's ToolUseStart/ToolResult).
    fun respondPermission(allow: Boolean) {
        val pending = _state.value.pendingPermission ?: return
        client?.send(UiEvent.PermissionResponse(pending.toolUseId, allow))
        _state.update { it.copy(pendingPermission = null) }
    }

    private fun listener(epoch: Int) = object : RelayClient.Listener {
        // False once we've connected elsewhere or detached: drop this socket's callbacks.
        private fun live() = epoch == generation

        override fun onAttached() {
            if (!live()) return
            reconnectAttempts = 0 // a clean attach resets the backoff budget
            _state.update { it.copy(connection = Connection.Attached, status = "Attached") }
        }

        override fun onBusy() {
            if (live()) _state.update {
                it.copy(connection = Connection.Busy, status = "Busy — another controller holds the session")
            }
        }

        override fun onEvent(seq: Long, event: ControllerEvent) {
            if (!live()) return
            when (event) {
                is ControllerEvent.UserMessage -> append(Role.User, event.text)
                is ControllerEvent.AssistantText -> append(Role.Assistant, event.text)
                is ControllerEvent.AssistantThinking -> append(Role.Thinking, event.text)
                is ControllerEvent.ToolUseStart ->
                    append(Role.ToolCall, event.name, body = event.summary.ifBlank { null })
                is ControllerEvent.ToolResult -> append(
                    Role.ToolResult,
                    if (event.isError) "Error" else "Result",
                    body = event.content.ifBlank { null },
                    isError = event.isError,
                )
                is ControllerEvent.PermissionRequest -> _state.update {
                    it.copy(pendingPermission = PendingPermission(event.toolUseId, event.toolName, event.summary))
                }
                is ControllerEvent.PermissionResolved -> {
                    append(Role.System, "permission ${if (event.allow) "allowed" else "denied"}: ${event.toolName}")
                    // Clear the prompt even if it was resolved elsewhere (another controller).
                    _state.update { it.copy(pendingPermission = null) }
                }
                is ControllerEvent.Notice -> append(Role.System, event.text)
                is ControllerEvent.Warn -> append(Role.System, "⚠ ${event.text}")
                is ControllerEvent.Error -> {
                    append(Role.System, "error: ${event.message}")
                    _state.update { it.copy(turnInFlight = false) }
                }
                ControllerEvent.TurnComplete -> _state.update { it.copy(turnInFlight = false) }
                ControllerEvent.MaxIterations -> {
                    append(Role.System, "max iterations reached")
                    _state.update { it.copy(turnInFlight = false) }
                }
                is ControllerEvent.SessionInfo -> _state.update {
                    it.copy(
                        sessionInfo = SessionContext(
                            event.model,
                            event.cwd,
                            event.gitBranch,
                            event.sessionId,
                            event.sessionName,
                        ),
                    )
                }
                is ControllerEvent.Usage -> Unit // not surfaced in the UI yet
            }
        }

        override fun onClosed(code: Int, reason: String) {
            if (!live()) return
            if (wantConnected) scheduleReconnect()
            else _state.update {
                it.copy(connection = Connection.Disconnected, status = "Disconnected ($code)", turnInFlight = false)
            }
        }

        override fun onFailure(error: Throwable) {
            if (!live()) return
            if (wantConnected) scheduleReconnect()
            else _state.update {
                it.copy(connection = Connection.Error, status = "Connection failed: ${error.message}", turnInFlight = false)
            }
        }
    }

    // A dropped socket while we still want to be connected: redial with capped
    // exponential backoff, resuming from lastSeq so the daemon replays only the events
    // missed during the gap (the transcript is kept, not cleared). Gives up after
    // MAX_RECONNECT_ATTEMPTS by parking in the detached state (still bound) so the user
    // can tap Reattach to retry, or Disconnect to switch — rather than hammering a dead box.
    private fun scheduleReconnect() {
        if (!wantConnected) return
        if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
            wantConnected = false
            prefs.edit().putBoolean(KEY_PAUSED, true).apply() // stay bound, stop hammering
            _state.update {
                it.copy(connection = Connection.Detached, status = "Couldn't reconnect — tap Reattach", turnInFlight = false)
            }
            return
        }
        val attempt = reconnectAttempts++
        val backoffMs = 1000L shl attempt.coerceAtMost(3) // 1s, 2s, 4s, 8s, 8s…
        _state.update {
            it.copy(connection = Connection.Connecting, status = "Reconnecting… (${attempt + 1})")
        }
        reconnectJob?.cancel()
        reconnectJob = viewModelScope.launch {
            delay(backoffMs)
            if (wantConnected) dial(client?.lastSeq) // resume cursor → replay only missed events
        }
    }

    // A default-network change (wifi↔cellular) strands the current socket on a dead
    // interface that OkHttp wouldn't notice until its ~20s ping times out. Redial now over
    // the new network, with a fresh attempt budget, rather than waiting for that timeout.
    private fun forceReconnect() {
        if (!wantConnected) return
        reconnectJob?.cancel()
        reconnectAttempts = 0
        _state.update { it.copy(connection = Connection.Connecting, status = "Reconnecting…") }
        dial(client?.lastSeq)
    }

    private fun onNetworkAvailable(network: Network) {
        val previous = lastNetwork
        lastNetwork = network
        // The first callback (at registration) and repeats of the same network are no-ops;
        // a *changed* default network while we want to be connected triggers an instant redial.
        if (previous != null && previous != network && wantConnected) forceReconnect()
    }

    private fun append(role: Role, text: String, body: String? = null, isError: Boolean = false) =
        _state.update { it.copy(lines = it.lines + ChatLine(nextId++, role, text, body, isError)) }

    override fun onCleared() {
        connectivityManager?.unregisterNetworkCallback(networkCallback)
        wantConnected = false
        reconnectJob?.cancel()
        client?.detach()
        client?.close()
    }

    private companion object {
        const val MAX_RECONNECT_ATTEMPTS = 6
        const val PREFS = "nudge"
        const val KEY_CODE = "pairing_code"
        const val KEY_PAUSED = "paused" // bound-but-detached: don't auto-reconnect on resume
    }
}
