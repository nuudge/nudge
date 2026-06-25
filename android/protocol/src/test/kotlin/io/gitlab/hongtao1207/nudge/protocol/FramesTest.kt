package io.gitlab.hongtao1207.nudge.protocol

import kotlin.test.Test
import kotlin.test.assertEquals

// These assertions pin the exact JSON bytes against serde's external tagging. If the
// Rust enum reorders fields or renames a variant, an exact-string check here breaks
// loudly — far better than a silent wire mismatch discovered on a real phone.
class FramesTest {
    private inline fun <reified T> enc(s: kotlinx.serialization.KSerializer<T>, v: T) =
        WireJson.encodeToString(s, v)

    @Test
    fun clientFrameUnitVariantIsBareString() {
        assertEquals("\"Detach\"", enc(ClientFrame.serializer(), ClientFrame.Detach))
    }

    @Test
    fun clientFrameAttachStructVariant() {
        assertEquals("""{"Attach":{"after_seq":null}}""", enc(ClientFrame.serializer(), ClientFrame.Attach(null)))
        assertEquals("""{"Attach":{"after_seq":7}}""", enc(ClientFrame.serializer(), ClientFrame.Attach(7)))
    }

    @Test
    fun clientFrameCommandIsNewtypeWrappingUiEvent() {
        assertEquals(
            """{"Command":{"UserMessage":{"text":"hi"}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Command(UiEvent.UserMessage("hi"))),
        )
        assertEquals(
            """{"Command":"Quit"}""",
            enc(ClientFrame.serializer(), ClientFrame.Command(UiEvent.Quit)),
        )
        assertEquals(
            """{"Command":{"PermissionResponse":{"tool_use_id":"t1","allow":true}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Command(UiEvent.PermissionResponse("t1", true))),
        )
        // RenameSession mirrors Rust's Option<String>: a name verbatim, or null to let
        // the daemon derive one. serde emits the `name` key either way (not omitted).
        assertEquals(
            """{"Command":{"RenameSession":{"name":"auth-fix"}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Command(UiEvent.RenameSession("auth-fix"))),
        )
        assertEquals(
            """{"Command":{"RenameSession":{"name":null}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Command(UiEvent.RenameSession(null))),
        )
    }

    @Test
    fun serverFrameDecodesEventAndUnits() {
        val attached = WireJson.decodeFromString(ServerFrame.serializer(), "\"Attached\"")
        assertEquals(ServerFrame.Attached, attached)
        val busy = WireJson.decodeFromString(ServerFrame.serializer(), "\"Busy\"")
        assertEquals(ServerFrame.Busy, busy)

        val ev = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":0,"event":{"AssistantText":{"text":"hi"}}}}""",
        )
        assertEquals(ServerFrame.Event(0, ControllerEvent.AssistantText("hi")), ev)
    }

    @Test
    fun serverFrameDecodesRichControllerEvents() {
        val toolResult = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":4,"event":{"ToolResult":{"id":"t1","content":"line one\nline two","is_error":false}}}}""",
        )
        assertEquals(
            ServerFrame.Event(4, ControllerEvent.ToolResult("t1", "line one\nline two", false)),
            toolResult,
        )

        val perm = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":5,"event":{"PermissionRequest":{"tool_use_id":"x","tool_name":"Bash","summary":"ls"}}}}""",
        )
        assertEquals(
            ServerFrame.Event(5, ControllerEvent.PermissionRequest("x", "Bash", "ls")),
            perm,
        )

        val usage = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":6,"event":{"Usage":{"in_tokens":10,"out_tokens":20,"cache_write":1,"cache_read":2}}}}""",
        )
        assertEquals(ServerFrame.Event(6, ControllerEvent.Usage(10, 20, 1, 2)), usage)
    }

    @Test
    fun controllerEventRoundTrips() {
        val events = listOf(
            ControllerEvent.TurnComplete,
            ControllerEvent.MaxIterations,
            ControllerEvent.AssistantThinking("pondering"),
            ControllerEvent.PermissionResolved("Bash", true),
            ControllerEvent.UserMessage("hello"),
            ControllerEvent.Notice("note"),
            ControllerEvent.Warn("careful"),
            ControllerEvent.Error("boom"),
        )
        for (e in events) {
            val json = enc(ControllerEvent.serializer(), e)
            val back = WireJson.decodeFromString(ControllerEvent.serializer(), json)
            assertEquals(e, back, "round-trip changed $e (json=$json)")
        }
    }

    @Test
    fun clientFrameRoundTrips() {
        val frames = listOf(
            ClientFrame.Attach(null),
            ClientFrame.Attach(42),
            ClientFrame.Detach,
            ClientFrame.Command(UiEvent.UserMessage("go")),
            ClientFrame.Command(UiEvent.SetModel("claude-opus-4-8")),
            ClientFrame.Command(UiEvent.RenameSession("my-name")),
            ClientFrame.Command(UiEvent.RenameSession(null)),
            ClientFrame.Command(UiEvent.ListServers),
            ClientFrame.Command(UiEvent.Quit),
        )
        for (f in frames) {
            val json = enc(ClientFrame.serializer(), f)
            val back = WireJson.decodeFromString(ClientFrame.serializer(), json)
            assertEquals(f, back, "round-trip changed $f (json=$json)")
        }
    }

    @Test
    fun sessionInfoDecodesAndRoundTrips() {
        // Exact bytes serde emits for ControllerEvent::SessionInfo (snake_case keys; the
        // header is the only consumer, so a drift here would break the phone silently).
        val withBranch = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":0,"event":{"SessionInfo":{"model":"claude-opus-4-8","cwd":"~/proj","git_branch":"main","session_id":"7f3a"}}}}""",
        )
        assertEquals(
            ServerFrame.Event(0, ControllerEvent.SessionInfo("claude-opus-4-8", "~/proj", "main", "7f3a")),
            withBranch,
        )

        // git_branch is an Option<String> in Rust → null when the cwd isn't a git repo.
        val noBranch = WireJson.decodeFromString(
            ControllerEvent.serializer(),
            """{"SessionInfo":{"model":"m","cwd":"/tmp","git_branch":null,"session_id":"id"}}""",
        )
        assertEquals(ControllerEvent.SessionInfo("m", "/tmp", null, "id"), noBranch)

        for (e in listOf(
            ControllerEvent.SessionInfo("m", "/tmp", "dev", "id"),
            ControllerEvent.SessionInfo("m", "/tmp", null, "id"),
        )) {
            val json = enc(ControllerEvent.serializer(), e)
            assertEquals(e, WireJson.decodeFromString(ControllerEvent.serializer(), json), "round-trip changed $e")
        }
    }
}
