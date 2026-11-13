package io.gitlab.hongtao1207.nudge.protocol

import kotlin.test.Test
import kotlin.test.assertEquals

// Exact-string checks pin the JSON bytes serde emits, so Rust-side drift fails here.
class FramesTest {
    private inline fun <reified T> enc(s: kotlinx.serialization.KSerializer<T>, v: T) =
        WireJson.encodeToString(s, v)

    @Test
    fun clientFrameUnitVariantIsBareString() {
        assertEquals("\"Detach\"", enc(ClientFrame.serializer(), ClientFrame.Detach))
    }

    @Test
    fun clientFrameAttachStructVariant() {
        val who = ClientIdentity.human("alice")
        assertEquals(
            """{"Attach":{"after_seq":null,"who":{"kind":"Human","name":"alice","session_id":null,"task":null}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Attach(null, who)),
        )
        assertEquals(
            """{"Attach":{"after_seq":7,"who":{"kind":"Human","name":"alice","session_id":null,"task":null}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Attach(7, who)),
        )
    }

    @Test
    fun userMessageCarriesSender() {
        assertEquals(
            """{"UserMessage":{"text":"hi","sender":"alice"}}""",
            enc(ControllerEvent.serializer(), ControllerEvent.UserMessage("hi", "alice")),
        )
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
        // Frame wrapper and UiEvent are both named "Command", so the tag nests.
        assertEquals(
            """{"Command":{"Command":{"line":"/model x"}}}""",
            enc(ClientFrame.serializer(), ClientFrame.Command(UiEvent.Command("/model x"))),
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
            ControllerEvent.UserMessage("hello", "alice"),
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
            ClientFrame.Attach(null, ClientIdentity.human("alice")),
            ClientFrame.Attach(42, ClientIdentity.human("alice")),
            ClientFrame.Detach,
            ClientFrame.Command(UiEvent.UserMessage("go")),
            ClientFrame.Command(UiEvent.Command("/model claude-opus-4-8")),
            ClientFrame.Command(UiEvent.Command("/session-rename my-name")),
            ClientFrame.Command(UiEvent.Command("/mcp")),
            ClientFrame.Command(UiEvent.Quit),
        )
        for (f in frames) {
            val json = enc(ClientFrame.serializer(), f)
            val back = WireJson.decodeFromString(ClientFrame.serializer(), json)
            assertEquals(f, back, "round-trip changed $f (json=$json)")
        }
    }

    @Test
    fun capabilitiesDecodesAndRoundTrips() {
        val decoded = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":1,"event":{"Capabilities":{"commands":[{"name":"/model","usage":"u"}],"models":[{"id":"id1","label":"L1"}],"mcp":[{"name":"gitlab","description":"d","loaded":false}],"peers":[{"name":"child-ab12cd34","kind":"Agent","supervised":true,"session_id":"s1","task":null}]}}}}""",
        )
        assertEquals(
            ServerFrame.Event(
                1,
                ControllerEvent.Capabilities(
                    listOf(CommandInfo("/model", "u")),
                    listOf(ModelInfo("id1", "L1")),
                    listOf(McpServerInfo("gitlab", "d", false)),
                    listOf(PeerInfo("child-ab12cd34", ClientKind.Agent, true, "s1", null)),
                ),
            ),
            decoded,
        )

        val cap = ControllerEvent.Capabilities(
            listOf(CommandInfo("/mcp", "usage")),
            listOf(ModelInfo("m", "Model")),
            listOf(McpServerInfo("everything", null, true)),
            emptyList(),
        )
        assertEquals(cap, WireJson.decodeFromString(ControllerEvent.serializer(), enc(ControllerEvent.serializer(), cap)))

        // An older daemon omits `peers` entirely.
        val old = WireJson.decodeFromString(
            ControllerEvent.serializer(),
            """{"Capabilities":{"commands":[],"models":[],"mcp":[]}}""",
        )
        assertEquals(ControllerEvent.Capabilities(emptyList(), emptyList(), emptyList(), emptyList()), old)
    }

    @Test
    fun sessionInfoDecodesAndRoundTrips() {
        val named = WireJson.decodeFromString(
            ServerFrame.serializer(),
            """{"Event":{"seq":0,"event":{"SessionInfo":{"model":"claude-opus-4-8","cwd":"~/proj","git_branch":"main","session_id":"7f3a","session_name":"auth-fix","auto":true}}}}""",
        )
        assertEquals(
            ServerFrame.Event(0, ControllerEvent.SessionInfo("claude-opus-4-8", "~/proj", "main", "7f3a", "auth-fix", true)),
            named,
        )

        // serde emits None as an explicit null key, not omitted.
        val bare = WireJson.decodeFromString(
            ControllerEvent.serializer(),
            """{"SessionInfo":{"model":"m","cwd":"/tmp","git_branch":null,"session_id":"id","session_name":null,"auto":false}}""",
        )
        assertEquals(ControllerEvent.SessionInfo("m", "/tmp", null, "id", null, false), bare)

        // An older daemon omits session_name and auto.
        val legacy = WireJson.decodeFromString(
            ControllerEvent.serializer(),
            """{"SessionInfo":{"model":"m","cwd":"/tmp","git_branch":null,"session_id":"id"}}""",
        )
        assertEquals(ControllerEvent.SessionInfo("m", "/tmp", null, "id", null, false), legacy)

        for (e in listOf(
            ControllerEvent.SessionInfo("m", "/tmp", "dev", "id", "my-label", true),
            ControllerEvent.SessionInfo("m", "/tmp", null, "id", null, false),
        )) {
            val json = enc(ControllerEvent.serializer(), e)
            assertEquals(e, WireJson.decodeFromString(ControllerEvent.serializer(), json), "round-trip changed $e")
        }
    }
}
