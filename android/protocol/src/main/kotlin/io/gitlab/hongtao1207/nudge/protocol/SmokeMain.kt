package io.gitlab.hongtao1207.nudge.protocol

import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.system.exitProcess

// Headless smoke driver for 8.4-a: attach to a relay-paired daemon, send one message,
// print the streamed response, and exit on TurnComplete. Proves the whole protocol
// kit end-to-end over the *real* relay (WS dial → E2E → attach handshake → live events)
// before any Android UI exists. Run via the Gradle `:protocol:smoke` task:
//   ./gradlew :protocol:smoke --args "nudge:<pairing-code>"
// Optional second arg overrides the prompt. Permission requests are auto-denied so the
// smoke can never mutate anything on the host.
fun main(args: Array<String>) {
    require(args.isNotEmpty()) {
        "usage: smoke <nudge:pairing-code> [message]"
    }
    val pairing = Pairing.decode(args[0])
    val message = args.getOrNull(1) ?: "Reply with exactly five words and use no tools."
    println("dialing ${pairing.dialUrl()}")

    val done = CountDownLatch(1)
    lateinit var client: RelayClient
    client = RelayClient(
        pairing,
        object : RelayClient.Listener {
            override fun onAttached() {
                println("[attached] sending: $message")
                client.send(UiEvent.UserMessage(message))
            }

            override fun onBusy() {
                println("[busy] another controller already holds the session")
                done.countDown()
            }

            override fun onEvent(seq: Long, event: ControllerEvent) {
                when (event) {
                    is ControllerEvent.AssistantText -> println("[$seq] assistant: ${event.text}")
                    is ControllerEvent.AssistantThinking -> println("[$seq] (thinking) ${event.text}")
                    is ControllerEvent.ToolUseStart -> println("[$seq] tool ${event.name}: ${event.summary}")
                    is ControllerEvent.ToolResult ->
                        println("[$seq] tool result (${if (event.isError) "error" else "ok"})")
                    is ControllerEvent.PermissionRequest -> {
                        println("[$seq] permission asked for ${event.toolName} — auto-denying in smoke")
                        client.send(UiEvent.PermissionResponse(event.toolUseId, false))
                    }
                    is ControllerEvent.Notice -> println("[$seq] notice: ${event.text}")
                    is ControllerEvent.Warn -> println("[$seq] warn: ${event.text}")
                    is ControllerEvent.Error -> {
                        println("[$seq] ERROR: ${event.message}")
                        done.countDown()
                    }
                    ControllerEvent.TurnComplete -> {
                        println("[$seq] turn complete")
                        done.countDown()
                    }
                    else -> println("[$seq] $event")
                }
            }

            override fun onClosed(code: Int, reason: String) {
                println("[closed] $code $reason")
            }

            override fun onFailure(error: Throwable) {
                println("[failure] ${error.message}")
                done.countDown()
            }
        },
    )

    client.connect()
    if (!done.await(120, TimeUnit.SECONDS)) {
        println("[timeout] no TurnComplete within 120s")
    }
    client.detach()
    client.close()
    // OkHttp's dispatcher holds non-daemon threads; exit explicitly so the task ends.
    exitProcess(0)
}
