package io.gitlab.hongtao1207.nudge.app

import android.app.Application
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.os.Build
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import io.gitlab.hongtao1207.nudge.protocol.ClientIdentity
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

data class ChatLine(
    val id: Long,
    val role: Role,
    val text: String,
    val body: String? = null,
    val isError: Boolean = false,
    // Null for our own turns, which render without a name label.
    val sender: String? = null,
)

data class PendingPermission(val toolUseId: String, val toolName: String, val summary: String)

data class SessionContext(
    val model: String,
    val cwd: String,
    val gitBranch: String?,
    val sessionId: String,
    val sessionName: String?,
    val auto: Boolean,
)

data class ChatUiState(
    val connection: Connection = Connection.Disconnected,
    val status: String = "Not connected",
    val lines: List<ChatLine> = emptyList(),
    val turnInFlight: Boolean = false,
    val pendingPermission: PendingPermission? = null,
    val sessionInfo: SessionContext? = null,
    val capabilities: ControllerEvent.Capabilities? = null,
)

// Listener callbacks fire on OkHttp's thread; MutableStateFlow updates are thread-safe, so
// no marshalling. User bubbles come from the daemon's UserMessage echo, so replay rebuilds
// the transcript with no client-side bookkeeping.
class ChatViewModel(app: Application) : AndroidViewModel(app) {
    private val _state = MutableStateFlow(ChatUiState())
    val state: StateFlow<ChatUiState> = _state.asStateFlow()

    // The pairing code embeds the E2E key, so this persists a secret; it's app-private
    // and allowBackup=false.
    private val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    private var client: RelayClient? = null
    private var pairing: Pairing? = null
    private var nextId = 0L

    private val identity = ClientIdentity.human(Build.MODEL)

    // A wifi↔cellular handoff strands the socket until OkHttp's ping notices (~20-40s);
    // watching the default network redials immediately. Callbacks arrive off main.
    private val connectivityManager = app.getSystemService(ConnectivityManager::class.java)
    private var lastNetwork: Network? = null
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            viewModelScope.launch { onNetworkAvailable(network) }
        }
    }

    // Bumped on every connect and detach; a listener ignores callbacks from a past epoch so
    // a stale socket can't clobber the UI. Read on OkHttp's thread, hence @Volatile.
    @Volatile
    private var generation = 0

    // True between connect() and an intentional detach; a drop while set triggers a redial.
    @Volatile
    private var wantConnected = false
    private var reconnectAttempts = 0
    private var reconnectJob: Job? = null

    init {
        connectivityManager?.registerDefaultNetworkCallback(networkCallback)
        if (prefs.getString(KEY_CODE, null) != null && prefs.getBoolean(KEY_PAUSED, false)) {
            _state.update { it.copy(connection = Connection.Detached, status = "Detached") }
        }
    }

    fun connect(code: String) {
        reconnectJob?.cancel()
        // A plain connect does a full replay, so start empty to avoid doubling lines.
        _state.update {
            it.copy(
                connection = Connection.Connecting,
                status = "Connecting…",
                lines = emptyList(),
                pendingPermission = null,
                sessionInfo = null,
                capabilities = null,
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
        prefs.edit().putString(KEY_CODE, code).remove(KEY_PAUSED).apply()
        dial(afterSeq = null)
    }

    private fun dial(afterSeq: Long?) {
        val p = pairing ?: return
        client?.close()
        generation++
        client = RelayClient(p, listener(generation), identity).also { it.connect(afterSeq) }
    }

    // A paused (user-detached) pairing waits for a manual Reattach instead of reconnecting.
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

    // Stays bound (pairing kept, marked paused); the agent keeps running headless.
    fun detach() {
        wantConnected = false
        reconnectJob?.cancel()
        prefs.edit().putBoolean(KEY_PAUSED, true).apply()
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

    fun reattach() {
        prefs.getString(KEY_CODE, null)?.let { connect(it) }
    }

    // Forgets the pairing. A controller can't end the session; only stopping the daemon can.
    fun disconnect() {
        wantConnected = false
        reconnectJob?.cancel()
        prefs.edit().remove(KEY_CODE).remove(KEY_PAUSED).apply()
        generation++
        client?.detach()
        client?.close()
        client = null
        _state.update {
            it.copy(
                connection = Connection.Disconnected,
                status = "Disconnected",
                lines = emptyList(),
                turnInFlight = false,
                pendingPermission = null,
                sessionInfo = null,
                capabilities = null,
            )
        }
    }

    fun send(text: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty() || client == null) return
        // Like the TUI, a single-line `/…` is a server-parsed command, not a turn: no
        // TurnComplete will come to clear turnInFlight.
        if (trimmed.startsWith("/") && !trimmed.contains("\n")) {
            client?.send(UiEvent.Command(trimmed))
            return
        }
        _state.update { it.copy(turnInFlight = true) }
        client?.send(UiEvent.UserMessage(trimmed))
    }

    fun selectModel(id: String) {
        client?.send(UiEvent.Command("/model $id"))
    }

    fun respondPermission(allow: Boolean) {
        val pending = _state.value.pendingPermission ?: return
        client?.send(UiEvent.PermissionResponse(pending.toolUseId, allow))
        _state.update { it.copy(pendingPermission = null) }
    }

    private fun listener(epoch: Int) = object : RelayClient.Listener {
        private fun live() = epoch == generation

        override fun onAttached() {
            if (!live()) return
            reconnectAttempts = 0
            _state.update { it.copy(connection = Connection.Attached, status = "Attached") }
        }

        override fun onBusy() {
            // The daemon is multi-attach, so Busy means no live session, not a held one.
            if (live()) _state.update {
                it.copy(connection = Connection.Busy, status = "The daemon has no live session")
            }
        }

        override fun onEvent(seq: Long, event: ControllerEvent) {
            if (!live()) return
            when (event) {
                is ControllerEvent.UserMessage -> {
                    val from = event.sender.takeIf { it.isNotEmpty() && it != identity.name }
                    append(Role.User, event.text, sender = from)
                }
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
                    // May have been resolved by another controller.
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
                            event.auto,
                        ),
                    )
                }
                is ControllerEvent.Capabilities -> _state.update { it.copy(capabilities = event) }
                is ControllerEvent.Usage -> Unit
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

    // Resumes from lastSeq so only missed events replay; after MAX_RECONNECT_ATTEMPTS it
    // parks in Detached (still bound) rather than hammering a dead host.
    private fun scheduleReconnect() {
        if (!wantConnected) return
        if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
            wantConnected = false
            prefs.edit().putBoolean(KEY_PAUSED, true).apply()
            _state.update {
                it.copy(connection = Connection.Detached, status = "Couldn't reconnect — tap Reattach", turnInFlight = false)
            }
            return
        }
        val attempt = reconnectAttempts++
        val backoffMs = 1000L shl attempt.coerceAtMost(3)
        _state.update {
            it.copy(connection = Connection.Connecting, status = "Reconnecting… (${attempt + 1})")
        }
        reconnectJob?.cancel()
        reconnectJob = viewModelScope.launch {
            delay(backoffMs)
            if (wantConnected) dial(client?.lastSeq)
        }
    }

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
        // The first callback fires at registration; only a changed network should redial.
        if (previous != null && previous != network && wantConnected) forceReconnect()
    }

    private fun append(
        role: Role,
        text: String,
        body: String? = null,
        isError: Boolean = false,
        sender: String? = null,
    ) = _state.update {
        it.copy(lines = it.lines + ChatLine(nextId++, role, text, body, isError, sender))
    }

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
        const val KEY_PAUSED = "paused" // bound but detached: don't auto-reconnect on resume
    }
}
