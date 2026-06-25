package io.gitlab.hongtao1207.nudge.protocol

import kotlinx.serialization.DeserializationStrategy
import kotlinx.serialization.KSerializer
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.SerializationStrategy
import kotlinx.serialization.descriptors.SerialDescriptor
import kotlinx.serialization.descriptors.buildClassSerialDescriptor
import kotlinx.serialization.encoding.Decoder
import kotlinx.serialization.encoding.Encoder
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonDecoder
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonEncoder
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive

// The Kotlin peer of the Rust wire protocol (core::events + transport::wire). These
// types must serialize byte-compatibly with serde's *external* tagging, which is NOT
// what kotlinx.serialization produces for sealed classes by default (it uses an
// internal "type" discriminator). So each enum carries a hand-written serializer:
//   - unit variant      -> the bare string  "Detach"
//   - struct variant     -> {"Attach": {"after_seq": 7}}
//   - newtype variant    -> {"Command": <inner>}   (inner serialized on its own)
// Snake_case field names match Rust via @SerialName; u64 maps to Long, Option<u64>
// to a nullable Long (serde emits the field as null when None).

// The compact JSON codec for frames — matches serde_json::to_vec (no whitespace).
// encodeDefaults so any field with a default still goes on the wire, as serde does.
val WireJson: Json = Json {
    ignoreUnknownKeys = true
    encodeDefaults = true
}

// ── external-tagging helpers ─────────────────────────────────────────────────
private fun emitTag(encoder: JsonEncoder, tag: String) {
    encoder.encodeJsonElement(JsonPrimitive(tag))
}

private fun <T> emitTagged(encoder: JsonEncoder, tag: String, ser: SerializationStrategy<T>, value: T) {
    encoder.encodeJsonElement(JsonObject(mapOf(tag to encoder.json.encodeToJsonElement(ser, value))))
}

// Split an externally-tagged value into (variant tag, inner element-or-null). A bare
// string is a unit variant (null inner); a single-key object is a data variant.
private fun splitTagged(decoder: JsonDecoder): Pair<String, JsonElement?> =
    when (val el = decoder.decodeJsonElement()) {
        is JsonPrimitive -> {
            require(el.isString) { "expected an externally-tagged enum, got $el" }
            el.content to null
        }
        is JsonObject -> {
            require(el.size == 1) { "externally-tagged enum object needs exactly one key, got ${el.keys}" }
            val (k, v) = el.entries.single()
            k to v
        }
        else -> error("unexpected JSON for an externally-tagged enum: $el")
    }

// ── ControllerEvent (core -> client) ─────────────────────────────────────────
@Serializable(with = ControllerEventSerializer::class)
sealed class ControllerEvent {
    @Serializable
    data class SessionInfo(
        val model: String,
        val cwd: String,
        @SerialName("git_branch") val gitBranch: String?,
        @SerialName("session_id") val sessionId: String,
        // Human label set via rename, null when the session is nameless. Defaulted so
        // a daemon that predates the field still decodes. The header prefers it over
        // the uuid. serde emits it as null (not absent) when None, so it's normally present.
        @SerialName("session_name") val sessionName: String? = null,
    ) : ControllerEvent()

    @Serializable
    data class Usage(
        @SerialName("in_tokens") val inTokens: Long,
        @SerialName("out_tokens") val outTokens: Long,
        @SerialName("cache_write") val cacheWrite: Long,
        @SerialName("cache_read") val cacheRead: Long,
    ) : ControllerEvent()

    @Serializable
    data class AssistantText(val text: String) : ControllerEvent()

    @Serializable
    data class AssistantThinking(val text: String) : ControllerEvent()

    @Serializable
    data class ToolUseStart(val id: String, val name: String, val summary: String) : ControllerEvent()

    @Serializable
    data class PermissionRequest(
        @SerialName("tool_use_id") val toolUseId: String,
        @SerialName("tool_name") val toolName: String,
        val summary: String,
    ) : ControllerEvent()

    @Serializable
    data class PermissionResolved(
        @SerialName("tool_name") val toolName: String,
        val allow: Boolean,
    ) : ControllerEvent()

    @Serializable
    data class ToolResult(
        val id: String,
        val content: String,
        @SerialName("is_error") val isError: Boolean,
    ) : ControllerEvent()

    @Serializable
    data class UserMessage(val text: String) : ControllerEvent()

    object TurnComplete : ControllerEvent()

    object MaxIterations : ControllerEvent()

    @Serializable
    data class Notice(val text: String) : ControllerEvent()

    @Serializable
    data class Warn(val text: String) : ControllerEvent()

    @Serializable
    data class Error(val message: String) : ControllerEvent()
}

object ControllerEventSerializer : KSerializer<ControllerEvent> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("ControllerEvent")

    override fun serialize(encoder: Encoder, value: ControllerEvent) {
        val j = encoder as? JsonEncoder ?: error("ControllerEvent requires a JSON encoder")
        when (value) {
            is ControllerEvent.SessionInfo -> emitTagged(j, "SessionInfo", ControllerEvent.SessionInfo.serializer(), value)
            is ControllerEvent.Usage -> emitTagged(j, "Usage", ControllerEvent.Usage.serializer(), value)
            is ControllerEvent.AssistantText -> emitTagged(j, "AssistantText", ControllerEvent.AssistantText.serializer(), value)
            is ControllerEvent.AssistantThinking -> emitTagged(j, "AssistantThinking", ControllerEvent.AssistantThinking.serializer(), value)
            is ControllerEvent.ToolUseStart -> emitTagged(j, "ToolUseStart", ControllerEvent.ToolUseStart.serializer(), value)
            is ControllerEvent.PermissionRequest -> emitTagged(j, "PermissionRequest", ControllerEvent.PermissionRequest.serializer(), value)
            is ControllerEvent.PermissionResolved -> emitTagged(j, "PermissionResolved", ControllerEvent.PermissionResolved.serializer(), value)
            is ControllerEvent.ToolResult -> emitTagged(j, "ToolResult", ControllerEvent.ToolResult.serializer(), value)
            is ControllerEvent.UserMessage -> emitTagged(j, "UserMessage", ControllerEvent.UserMessage.serializer(), value)
            ControllerEvent.TurnComplete -> emitTag(j, "TurnComplete")
            ControllerEvent.MaxIterations -> emitTag(j, "MaxIterations")
            is ControllerEvent.Notice -> emitTagged(j, "Notice", ControllerEvent.Notice.serializer(), value)
            is ControllerEvent.Warn -> emitTagged(j, "Warn", ControllerEvent.Warn.serializer(), value)
            is ControllerEvent.Error -> emitTagged(j, "Error", ControllerEvent.Error.serializer(), value)
        }
    }

    override fun deserialize(decoder: Decoder): ControllerEvent {
        val j = decoder as? JsonDecoder ?: error("ControllerEvent requires a JSON decoder")
        val (tag, inner) = splitTagged(j)
        fun <T> dec(ser: DeserializationStrategy<T>): T = j.json.decodeFromJsonElement(ser, inner!!)
        return when (tag) {
            "SessionInfo" -> dec(ControllerEvent.SessionInfo.serializer())
            "Usage" -> dec(ControllerEvent.Usage.serializer())
            "AssistantText" -> dec(ControllerEvent.AssistantText.serializer())
            "AssistantThinking" -> dec(ControllerEvent.AssistantThinking.serializer())
            "ToolUseStart" -> dec(ControllerEvent.ToolUseStart.serializer())
            "PermissionRequest" -> dec(ControllerEvent.PermissionRequest.serializer())
            "PermissionResolved" -> dec(ControllerEvent.PermissionResolved.serializer())
            "ToolResult" -> dec(ControllerEvent.ToolResult.serializer())
            "UserMessage" -> dec(ControllerEvent.UserMessage.serializer())
            "TurnComplete" -> ControllerEvent.TurnComplete
            "MaxIterations" -> ControllerEvent.MaxIterations
            "Notice" -> dec(ControllerEvent.Notice.serializer())
            "Warn" -> dec(ControllerEvent.Warn.serializer())
            "Error" -> dec(ControllerEvent.Error.serializer())
            else -> error("unknown ControllerEvent variant: $tag")
        }
    }
}

// ── UiEvent (client -> core) ─────────────────────────────────────────────────
@Serializable(with = UiEventSerializer::class)
sealed class UiEvent {
    @Serializable
    data class UserMessage(val text: String) : UiEvent()

    @Serializable
    data class SetModel(val model: String) : UiEvent()

    // Rename the session. name = null asks the daemon to derive one (git branch +
    // short id, else an LLM-suggested summary); a value is used verbatim. Mirrors the
    // Rust UiEvent::RenameSession { name: Option<String> }.
    @Serializable
    data class RenameSession(val name: String?) : UiEvent()

    @Serializable
    data class LoadServer(val name: String) : UiEvent()

    @Serializable
    data class UnloadServer(val name: String) : UiEvent()

    object ListServers : UiEvent()

    @Serializable
    data class PermissionResponse(
        @SerialName("tool_use_id") val toolUseId: String,
        val allow: Boolean,
    ) : UiEvent()

    object Quit : UiEvent()
}

object UiEventSerializer : KSerializer<UiEvent> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("UiEvent")

    override fun serialize(encoder: Encoder, value: UiEvent) {
        val j = encoder as? JsonEncoder ?: error("UiEvent requires a JSON encoder")
        when (value) {
            is UiEvent.UserMessage -> emitTagged(j, "UserMessage", UiEvent.UserMessage.serializer(), value)
            is UiEvent.SetModel -> emitTagged(j, "SetModel", UiEvent.SetModel.serializer(), value)
            is UiEvent.RenameSession -> emitTagged(j, "RenameSession", UiEvent.RenameSession.serializer(), value)
            is UiEvent.LoadServer -> emitTagged(j, "LoadServer", UiEvent.LoadServer.serializer(), value)
            is UiEvent.UnloadServer -> emitTagged(j, "UnloadServer", UiEvent.UnloadServer.serializer(), value)
            UiEvent.ListServers -> emitTag(j, "ListServers")
            is UiEvent.PermissionResponse -> emitTagged(j, "PermissionResponse", UiEvent.PermissionResponse.serializer(), value)
            UiEvent.Quit -> emitTag(j, "Quit")
        }
    }

    override fun deserialize(decoder: Decoder): UiEvent {
        val j = decoder as? JsonDecoder ?: error("UiEvent requires a JSON decoder")
        val (tag, inner) = splitTagged(j)
        fun <T> dec(ser: DeserializationStrategy<T>): T = j.json.decodeFromJsonElement(ser, inner!!)
        return when (tag) {
            "UserMessage" -> dec(UiEvent.UserMessage.serializer())
            "SetModel" -> dec(UiEvent.SetModel.serializer())
            "RenameSession" -> dec(UiEvent.RenameSession.serializer())
            "LoadServer" -> dec(UiEvent.LoadServer.serializer())
            "UnloadServer" -> dec(UiEvent.UnloadServer.serializer())
            "ListServers" -> UiEvent.ListServers
            "PermissionResponse" -> dec(UiEvent.PermissionResponse.serializer())
            "Quit" -> UiEvent.Quit
            else -> error("unknown UiEvent variant: $tag")
        }
    }
}

// ── ClientFrame (client -> daemon) ───────────────────────────────────────────
@Serializable(with = ClientFrameSerializer::class)
sealed class ClientFrame {
    @Serializable
    data class Attach(@SerialName("after_seq") val afterSeq: Long?) : ClientFrame()

    object Detach : ClientFrame()

    // Newtype variant: the inner UiEvent is serialized directly, not wrapped in a field.
    data class Command(val event: UiEvent) : ClientFrame()
}

object ClientFrameSerializer : KSerializer<ClientFrame> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("ClientFrame")

    override fun serialize(encoder: Encoder, value: ClientFrame) {
        val j = encoder as? JsonEncoder ?: error("ClientFrame requires a JSON encoder")
        when (value) {
            is ClientFrame.Attach -> emitTagged(j, "Attach", ClientFrame.Attach.serializer(), value)
            ClientFrame.Detach -> emitTag(j, "Detach")
            is ClientFrame.Command -> emitTagged(j, "Command", UiEvent.serializer(), value.event)
        }
    }

    override fun deserialize(decoder: Decoder): ClientFrame {
        val j = decoder as? JsonDecoder ?: error("ClientFrame requires a JSON decoder")
        val (tag, inner) = splitTagged(j)
        return when (tag) {
            "Attach" -> j.json.decodeFromJsonElement(ClientFrame.Attach.serializer(), inner!!)
            "Detach" -> ClientFrame.Detach
            "Command" -> ClientFrame.Command(j.json.decodeFromJsonElement(UiEvent.serializer(), inner!!))
            else -> error("unknown ClientFrame variant: $tag")
        }
    }
}

// ── ServerFrame (daemon -> client) ───────────────────────────────────────────
@Serializable(with = ServerFrameSerializer::class)
sealed class ServerFrame {
    object Attached : ServerFrame()

    object Busy : ServerFrame()

    @Serializable
    data class Event(val seq: Long, val event: ControllerEvent) : ServerFrame()
}

object ServerFrameSerializer : KSerializer<ServerFrame> {
    override val descriptor: SerialDescriptor = buildClassSerialDescriptor("ServerFrame")

    override fun serialize(encoder: Encoder, value: ServerFrame) {
        val j = encoder as? JsonEncoder ?: error("ServerFrame requires a JSON encoder")
        when (value) {
            ServerFrame.Attached -> emitTag(j, "Attached")
            ServerFrame.Busy -> emitTag(j, "Busy")
            is ServerFrame.Event -> emitTagged(j, "Event", ServerFrame.Event.serializer(), value)
        }
    }

    override fun deserialize(decoder: Decoder): ServerFrame {
        val j = decoder as? JsonDecoder ?: error("ServerFrame requires a JSON decoder")
        val (tag, inner) = splitTagged(j)
        return when (tag) {
            "Attached" -> ServerFrame.Attached
            "Busy" -> ServerFrame.Busy
            "Event" -> j.json.decodeFromJsonElement(ServerFrame.Event.serializer(), inner!!)
            else -> error("unknown ServerFrame variant: $tag")
        }
    }
}
