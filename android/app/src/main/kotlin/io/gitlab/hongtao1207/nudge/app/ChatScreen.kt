package io.gitlab.hongtao1207.nudge.app

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning

@Composable
fun ChatScreen(viewModel: ChatViewModel) {
    val state by viewModel.state.collectAsState()
    val listState = rememberLazyListState()

    // Follow the tail as new events stream in.
    androidx.compose.runtime.LaunchedEffect(state.lines.size) {
        if (state.lines.isNotEmpty()) listState.animateScrollToItem(state.lines.lastIndex)
    }

    // Pad only the top system bar here; the bottom inset (nav bar when the keyboard is
    // down, IME when it's up) is applied once on the input bar below as max(navBar, ime).
    // Chaining systemBarsPadding()+imePadding() here double-counted the nav-bar region
    // (the IME inset already spans it), floating the input far above the keyboard.
    Column(
        modifier = Modifier
            .fillMaxSize()
            .windowInsetsPadding(WindowInsets.statusBars),
    ) {
        StatusBar(
            state = state,
            onDetach = viewModel::detach,
            onDisconnect = viewModel::disconnect,
            onReattach = viewModel::reattach,
        )

        // Session context (from the daemon's SessionInfo event). Cleared on disconnect,
        // so it never shows on the pairing screen; kept while detached.
        state.sessionInfo?.let { ContextBar(it) }

        // The scanner/paste screen is only for the *unbound* states; a detached (bound)
        // session reattaches to the same daemon via the status-bar Reattach button.
        if (state.connection == Connection.Disconnected || state.connection == Connection.Error) {
            PairingRow(onConnect = viewModel::connect)
        }

        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .padding(horizontal = 8.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            items(state.lines, key = { it.id }) { line -> ChatRow(line) }
        }

        state.pendingPermission?.let { pending ->
            PermissionPrompt(
                pending = pending,
                onAllow = { viewModel.respondPermission(true) },
                onDeny = { viewModel.respondPermission(false) },
            )
        }

        if (state.turnInFlight) {
            LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
        }

        MessageInput(
            enabled = state.connection == Connection.Attached,
            onSend = viewModel::send,
        )
    }
}

@Composable
private fun StatusBar(
    state: ChatUiState,
    onDetach: () -> Unit,
    onDisconnect: () -> Unit,
    onReattach: () -> Unit,
) {
    val color = when (state.connection) {
        Connection.Attached -> MaterialTheme.colorScheme.primaryContainer
        Connection.Connecting -> MaterialTheme.colorScheme.secondaryContainer
        Connection.Busy -> MaterialTheme.colorScheme.tertiaryContainer
        Connection.Error -> MaterialTheme.colorScheme.errorContainer
        Connection.Detached, Connection.Disconnected -> MaterialTheme.colorScheme.surfaceVariant
    }
    Surface(color = color, modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            StatusIndicator(state.connection)
            Spacer(Modifier.width(8.dp))
            Text(
                text = state.status,
                style = MaterialTheme.typography.labelLarge,
                modifier = Modifier.weight(1f),
            )
            // Attached/connecting → Detach (pause, stay bound) or Disconnect (unbind).
            // Detached/busy → Reattach (same daemon) or Disconnect. Disconnected/Error use
            // the pairing screen, so no status-bar action there.
            when (state.connection) {
                Connection.Attached, Connection.Connecting -> {
                    TextButton(onClick = onDetach) { Text("Detach") }
                    TextButton(onClick = onDisconnect) { Text("Disconnect") }
                }
                Connection.Detached, Connection.Busy -> {
                    TextButton(onClick = onReattach) { Text("Reattach") }
                    TextButton(onClick = onDisconnect) { Text("Disconnect") }
                }
                Connection.Disconnected, Connection.Error -> {}
            }
        }
    }
}

// A spinner while a connection is in flight (covers both first connect and reconnect,
// which both sit in Connecting); otherwise a small status-colored dot so the connection
// state reads at a glance without parsing the label.
@Composable
private fun StatusIndicator(connection: Connection) {
    if (connection == Connection.Connecting) {
        CircularProgressIndicator(modifier = Modifier.size(14.dp), strokeWidth = 2.dp)
        return
    }
    val dot = when (connection) {
        Connection.Attached -> MaterialTheme.colorScheme.primary
        Connection.Busy -> MaterialTheme.colorScheme.tertiary
        Connection.Error -> MaterialTheme.colorScheme.error
        else -> MaterialTheme.colorScheme.outline // Detached / Disconnected
    }
    Box(modifier = Modifier.size(10.dp).clip(CircleShape).background(dot))
}

// Collapsible session context. The glance line (model · branch) is always shown and
// costs one thin line; tapping it reveals the full cwd + session label (with copy), which
// are reference info you rarely need at a glance on a phone. The label is the human name
// once renamed, else the uuid — both are valid `--resume` references.
@Composable
private fun ContextBar(info: SessionContext) {
    var expanded by rememberSaveable { mutableStateOf(false) }
    val clipboard = LocalClipboardManager.current
    val sessionLabel = info.sessionName ?: info.sessionId
    Surface(color = MaterialTheme.colorScheme.surfaceVariant, modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(horizontal = 12.dp, vertical = 4.dp)) {
            Row(
                modifier = Modifier.fillMaxWidth().clickable { expanded = !expanded },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = info.model + (info.gitBranch?.let { " · $it" } ?: ""),
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Text(
                    text = if (expanded) "⌃" else "⌄",
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (expanded) {
                Text(
                    text = info.cwd,
                    style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.padding(top = 2.dp),
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        text = sessionLabel,
                        style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(
                        onClick = { clipboard.setText(AnnotatedString(sessionLabel)) },
                        contentPadding = PaddingValues(horizontal = 8.dp, vertical = 0.dp),
                    ) {
                        Text("Copy", style = MaterialTheme.typography.labelMedium)
                    }
                }
            }
        }
    }
}

@Composable
private fun PairingRow(onConnect: (String) -> Unit) {
    val context = LocalContext.current
    // The scanner client is cheap to hold; restrict to QR so 1-D barcodes don't misread.
    val scanner = remember {
        GmsBarcodeScanning.getClient(
            context,
            GmsBarcodeScannerOptions.Builder()
                .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
                .build(),
        )
    }
    var code by rememberSaveable { mutableStateOf("") }
    var scanError by remember { mutableStateOf<String?>(null) }

    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(8.dp),
    ) {
        Button(
            onClick = {
                scanError = null
                scanner.startScan()
                    // rawValue is the decoded "nudge:…" string; connect() validates it.
                    .addOnSuccessListener { barcode -> barcode.rawValue?.let { onConnect(it.trim()) } }
                    .addOnCanceledListener { /* user backed out — no-op */ }
                    .addOnFailureListener { e -> scanError = e.message ?: "scan unavailable" }
            },
            modifier = Modifier.fillMaxWidth(),
        ) {
            Text("Scan QR code")
        }
        scanError?.let {
            Text(
                text = "$it — paste the code instead",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.error,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(top = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            OutlinedTextField(
                value = code,
                onValueChange = { code = it },
                label = { Text("…or paste nudge: code") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            Button(onClick = { onConnect(code.trim()) }, enabled = code.isNotBlank()) {
                Text("Connect")
            }
        }
    }
}

@Composable
private fun PermissionPrompt(pending: PendingPermission, onAllow: () -> Unit, onDeny: () -> Unit) {
    Surface(
        color = MaterialTheme.colorScheme.tertiaryContainer,
        shape = RoundedCornerShape(12.dp),
        modifier = Modifier
            .fillMaxWidth()
            .padding(8.dp),
    ) {
        Column(modifier = Modifier.padding(12.dp)) {
            Text("Allow tool: ${pending.toolName}?", style = MaterialTheme.typography.titleSmall)
            if (pending.summary.isNotBlank()) {
                // Bound the summary so a long command (e.g. a big bash invocation) scrolls
                // within the card instead of pushing the Allow/Deny buttons off-screen.
                MarkdownText(
                    text = pending.summary,
                    style = MaterialTheme.typography.bodyMedium,
                    modifier = Modifier
                        .padding(top = 4.dp)
                        .heightIn(max = 240.dp)
                        .verticalScroll(rememberScrollState()),
                )
            }
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(top = 8.dp),
                horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End),
            ) {
                TextButton(onClick = onDeny) { Text("Deny") }
                Button(onClick = onAllow) { Text("Allow") }
            }
        }
    }
}

@Composable
private fun MessageInput(enabled: Boolean, onSend: (String) -> Unit) {
    var msg by rememberSaveable { mutableStateOf("") }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            // Single bottom inset: nav bar when the keyboard is down, IME height when up.
            // The transcript (weight 1f) shrinks to make room; the input sits right on the
            // keyboard with no extra gap.
            .windowInsetsPadding(WindowInsets.navigationBars.union(WindowInsets.ime))
            .padding(8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        OutlinedTextField(
            value = msg,
            onValueChange = { msg = it },
            label = { Text(if (enabled) "Message" else "Connect to send") },
            enabled = enabled,
            modifier = Modifier.weight(1f),
        )
        Button(
            onClick = {
                onSend(msg)
                msg = ""
            },
            enabled = enabled && msg.isNotBlank(),
        ) {
            Text("Send")
        }
    }
}

@Composable
private fun ChatRow(line: ChatLine) {
    when (line.role) {
        Role.System -> Text(
            text = line.text,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
            modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp),
        )

        Role.Thinking -> Text(
            text = line.text,
            style = MaterialTheme.typography.bodySmall.copy(fontStyle = FontStyle.Italic),
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 2.dp),
        )

        Role.User -> Bubble(alignEnd = true, container = MaterialTheme.colorScheme.primaryContainer) {
            if (line.sender != null) {
                // Another party's turn in a shared session — label it with their name.
                Column(modifier = Modifier.padding(10.dp)) {
                    Text(
                        text = line.sender,
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Text(text = line.text)
                }
            } else {
                Text(text = line.text, modifier = Modifier.padding(10.dp))
            }
        }

        Role.Assistant -> Bubble(alignEnd = false, container = MaterialTheme.colorScheme.surfaceVariant) {
            MarkdownText(text = line.text, modifier = Modifier.padding(10.dp))
        }

        Role.ToolCall, Role.ToolResult -> ToolCard(line)
    }
}

@Composable
private fun Bubble(alignEnd: Boolean, container: androidx.compose.ui.graphics.Color, content: @Composable () -> Unit) {
    Box(modifier = Modifier.fillMaxWidth()) {
        Surface(
            color = container,
            shape = RoundedCornerShape(12.dp),
            modifier = Modifier
                .fillMaxWidth(0.92f)
                .align(if (alignEnd) Alignment.CenterEnd else Alignment.CenterStart),
        ) { content() }
    }
}

@Composable
private fun ToolCard(line: ChatLine) {
    val container =
        if (line.isError) MaterialTheme.colorScheme.errorContainer
        else MaterialTheme.colorScheme.secondaryContainer
    val accent =
        if (line.isError) MaterialTheme.colorScheme.error
        else MaterialTheme.colorScheme.onSecondaryContainer
    val label = when {
        line.role == Role.ToolCall -> "🔧 ${line.text}"
        line.isError -> "✗ ${line.text}"
        else -> "✓ ${line.text}"
    }
    Box(modifier = Modifier.fillMaxWidth()) {
        Surface(
            color = container,
            shape = RoundedCornerShape(12.dp),
            modifier = Modifier.fillMaxWidth(0.92f).align(Alignment.CenterStart),
        ) {
            Column(modifier = Modifier.padding(10.dp)) {
                Text(
                    text = label,
                    style = MaterialTheme.typography.labelLarge,
                    color = accent,
                )
                line.body?.let { CollapsibleMono(it) }
            }
        }
    }
}

// Tool output is often long; show a few lines and let the user expand. Horizontally
// scrollable so wide lines (paths, JSON) don't wrap into noise.
@Composable
private fun CollapsibleMono(text: String) {
    val lines = remember(text) { text.split("\n") }
    val needsToggle = lines.size > COLLAPSED_LINES || text.length > 240
    var expanded by remember(text) { mutableStateOf(false) }
    val shown = if (!needsToggle || expanded) text else lines.take(COLLAPSED_LINES).joinToString("\n")
    Text(
        text = shown,
        style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
        modifier = Modifier
            .padding(top = 4.dp)
            .horizontalScroll(rememberScrollState()),
    )
    if (needsToggle) {
        TextButton(
            onClick = { expanded = !expanded },
            contentPadding = PaddingValues(horizontal = 0.dp, vertical = 2.dp),
        ) {
            Text(if (expanded) "Show less" else "Show more", style = MaterialTheme.typography.labelMedium)
        }
    }
}

private const val COLLAPSED_LINES = 4
