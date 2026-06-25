use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::agent::{AgentConfig, Backend, run_agent};
use super::events::{AgentEvent, ControllerEvent, UiEvent};
use super::session::Session;
use crate::llm::{Message, Provider};

const CHANNEL_CAPACITY: usize = 64;

// A front-end's handle to the live session: it receives the controller event
// stream and sends UiEvents back. The front-end owns these ends; the broker owns
// the matching ends and mediates between them and the loop.
pub struct Controller {
    pub events: mpsc::Receiver<ControllerEvent>,
    pub ui_tx: mpsc::Sender<UiEvent>,
}

// Progress of the co-located relay dial that arms phone handoff on /background.
// Produced by the dial task (wired in the composition root, so `core` still names
// no transport) and consumed by the TUI to render the background pair screen.
#[derive(Clone, Debug)]
pub enum HandoffStatus {
    // Dialing the relay; the QR isn't useful yet.
    Connecting,
    // On the relay, waiting for a device — the QR is now scannable.
    Connected,
    // The dial failed (relay unset/unreachable); handoff is unavailable until the
    // next /background retries. Carries a short reason for the screen.
    Failed(String),
}

// A handle to a running session, abstracted over the transport the front-end
// talks to it through: an in-process `SessionHost` (default / daemon-host) or a
// `SocketClient` that proxies to a remote daemon over a Unix socket. `tui::run`
// is generic over this, so the front-end code is byte-for-byte identical whether
// it drives the loop directly or across a socket — the whole point of 8.1.
pub trait SessionHandle {
    // Bind a front-end, yielding its `Controller` (the replay+live event stream
    // and the UiEvent sink). `None` = the session is held elsewhere (single-attach
    // lock) or the transport is gone. The future is `Send` so a server can drive
    // it from a spawned task.
    fn attach(&self) -> impl Future<Output = Option<Controller>> + Send;
    // Bind a front-end, overriding the single-attach lock if one is already held
    // (local-TUI force-takeover — boots the current holder so the physically
    // present computer can reclaim a session a phone left attached). The default
    // delegates to `attach` (no force): a *remote* client must never be able to
    // force, so only the in-process `SessionHost` overrides this. `tui::run` calls
    // it on every foreground, so the local host forces and any remote `--connect`
    // TUI silently stays non-forcing.
    fn attach_force(&self) -> impl Future<Output = Option<Controller>> + Send {
        self.attach()
    }
    // Yield the front-end without ending the session (pause-in-place, e.g.
    // /background). Fire-and-forget: the loop keeps running headless and buffering;
    // a later `attach` replays the full history.
    fn detach(&self);
}

// The broker's view of one attached controller.
struct ControllerChannels {
    ui_rx: mpsc::Receiver<UiEvent>,
    event_tx: mpsc::Sender<ControllerEvent>,
}

// Control messages from `SessionHost`'s methods to the broker task.
enum HostCommand {
    // Bind a front-end. `force` overrides the single-attach lock (local-TUI
    // takeover); without it a second attach is refused. `ack` reports whether the
    // bind succeeded (false = busy).
    Attach {
        ui_rx: mpsc::Receiver<UiEvent>,
        event_tx: mpsc::Sender<ControllerEvent>,
        force: bool,
        ack: oneshot::Sender<bool>,
    },
    // Detach the current front-end without ending the session (pause-in-place).
    Detach,
    // End the session: the broker forwards a final UiEvent::Quit to the loop.
    Quit,
}

// Owns the running agent loop, the broker that connects front-ends to it, and
// the control channel into the broker. The loop's channels terminate at the
// broker (which lives for the whole session), so a front-end coming and going
// never closes them — the loop's lifetime is decoupled from any front-end and
// ends only on an explicit Quit.
pub struct SessionHost {
    agent_task: JoinHandle<Result<()>>,
    broker_task: JoinHandle<()>,
    ctl_tx: mpsc::UnboundedSender<HostCommand>,
    // Fired on every detach (every /background) to arm handoff: dial the relay so
    // a phone can attach. The composition root injects it (see `set_handoff_hook`)
    // so this module never names the transport — `core` stays a layer below
    // `transport`. The hook itself guards against re-dialing while one is already
    // live, so firing it each time just lets a failed dial be retried by
    // backgrounding again. `None` when no relay is configured (nothing to arm) and
    // in headless/daemon and remote modes.
    handoff_hook: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SessionHost {
    // Spawn the agent loop and a broker that mediates between it and front-ends.
    // Provider/Backend must be `Send + 'static` because the loop is spawned onto
    // the tokio runtime (their async methods already return Send futures by
    // design — see the Provider/Backend trait docs).
    pub fn spawn<P, B>(
        cfg: AgentConfig,
        provider: P,
        backend: B,
        session: Session,
        initial_messages: Vec<Message>,
        seed: Vec<ControllerEvent>,
    ) -> Self
    where
        P: Provider + Send + 'static,
        B: Backend + Send + 'static,
    {
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (loop_ui_tx, loop_ui_rx) = mpsc::channel(CHANNEL_CAPACITY);

        let agent_task = tokio::spawn(run_agent(
            cfg,
            provider,
            backend,
            session,
            initial_messages,
            loop_ui_rx,
            loop_agent_tx,
        ));

        // `seed` is the resumed transcript pre-translated to controller events
        // (empty for a fresh session). It primes the broker's replay buffer so
        // every controller — including a remote one that can't read the JSONL —
        // reconstructs the full history from the event stream alone.
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, seed));

        Self {
            agent_task,
            broker_task,
            ctl_tx,
            handoff_hook: None,
        }
    }

    // A cheap, cloneable handle to this host's broker (just its control channel).
    // It owns none of the loop, so it can be cloned freely and moved into spawned
    // tasks — e.g. the socket accept loop that serves remote front-ends.
    pub fn broker_handle(&self) -> BrokerHandle {
        BrokerHandle {
            ctl_tx: self.ctl_tx.clone(),
        }
    }

    // Install the enabler fired on every `detach` (every /background). Supplied by
    // the composition root so `core` carries no transport knowledge; in practice it
    // dials the relay so a phone can attach. The hook dedupes its own re-dialing, so
    // firing each time just retries after a failed dial. Not set when no relay is
    // configured, nor in daemon / remote (`SocketClient`) modes.
    pub fn set_handoff_hook(&mut self, hook: impl Fn() + Send + Sync + 'static) {
        self.handoff_hook = Some(Box::new(hook));
    }

    // End the session: ask the broker to deliver a final Quit to the loop, then
    // await both tasks. Idempotent — if the loop already ended (e.g. a controller
    // sent Quit), the control send is a no-op and the finished tasks return.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.ctl_tx.send(HostCommand::Quit);
        let loop_result = self.agent_task.await?;
        let _ = self.broker_task.await;
        loop_result
    }
}

impl SessionHandle for SessionHost {
    async fn attach(&self) -> Option<Controller> {
        self.broker_handle().attach().await
    }

    // The in-process host is the only `SessionHandle` that actually forces — it's
    // the local TUI, the one front-end allowed to boot a remote holder.
    async fn attach_force(&self) -> Option<Controller> {
        self.broker_handle().attach_force().await
    }

    fn detach(&self) {
        // Each /background arms handoff by firing the injected hook (it dials the
        // relay). The hook no-ops while a dial is already live, so firing every time
        // just lets a failed dial be retried by backgrounding again.
        if let Some(hook) = &self.handoff_hook {
            hook();
        }
        self.broker_handle().detach();
    }
}

// A cloneable handle to a session's broker, holding only its control channel. It
// is the single primitive both transports talk to the broker through: the
// in-process `SessionHost` delegates its `SessionHandle` impl here, and the daemon
// hands one to each socket connection. Being `Clone` + `'static`, it can be moved
// into spawned accept loops where a `&SessionHost` borrow couldn't reach.
#[derive(Clone)]
pub struct BrokerHandle {
    ctl_tx: mpsc::UnboundedSender<HostCommand>,
}

impl BrokerHandle {
    // Whether the broker task is still running (its receiver hasn't dropped). The
    // relay dial loop uses this to tell a force-takeover (broker alive — keep dialing
    // so a device can attach again) from a host shutdown (broker gone — stop).
    pub fn is_alive(&self) -> bool {
        !self.ctl_tx.is_closed()
    }

    // Shared body of attach / attach_force. `force` overrides the single-attach
    // lock (boots the current holder); without it a second attach is refused.
    async fn attach_inner(&self, force: bool) -> Option<Controller> {
        let (event_tx, event_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (ui_tx, ui_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .ctl_tx
            .send(HostCommand::Attach {
                ui_rx,
                event_tx,
                force,
                ack: ack_tx,
            })
            .is_err()
        {
            return None; // broker gone
        }
        match ack_rx.await {
            Ok(true) => Some(Controller {
                events: event_rx,
                ui_tx,
            }),
            _ => None, // busy, or broker gone
        }
    }
}

impl SessionHandle for BrokerHandle {
    // Returns `None` if another controller already holds the session (single-attach
    // mutual exclusion). On success the controller's event stream begins with a
    // replay of the full history, then live events.
    async fn attach(&self) -> Option<Controller> {
        self.attach_inner(false).await
    }

    // Boot the current holder and bind. Only ever reached via `SessionHost`
    // (the local TUI) — the daemon attaches remote clients with plain `attach`.
    async fn attach_force(&self) -> Option<Controller> {
        self.attach_inner(true).await
    }

    // Sent on the same control channel as attach, so a detach immediately followed
    // by an attach rebinds deterministically (FIFO) — no race against the
    // eventually-consistent drop-detach.
    fn detach(&self) {
        let _ = self.ctl_tx.send(HostCommand::Detach);
    }
}

// Translate a loop event into the controller-facing form, terminating a
// permission request's `oneshot`: the broker keeps the `Sender` (keyed by
// `tool_use_id`, with the tool name for later rendering) and fulfils it when the
// matching `PermissionResponse` arrives.
fn translate(
    ev: AgentEvent,
    pending: &mut HashMap<String, (oneshot::Sender<bool>, String)>,
) -> ControllerEvent {
    match ev {
        AgentEvent::SessionInfo {
            model,
            cwd,
            git_branch,
            session_id,
        } => ControllerEvent::SessionInfo {
            model,
            cwd,
            git_branch,
            session_id,
        },
        AgentEvent::Usage {
            in_tokens,
            out_tokens,
            cache_write,
            cache_read,
        } => ControllerEvent::Usage {
            in_tokens,
            out_tokens,
            cache_write,
            cache_read,
        },
        AgentEvent::AssistantText { text } => ControllerEvent::AssistantText { text },
        AgentEvent::AssistantThinking { text } => ControllerEvent::AssistantThinking { text },
        AgentEvent::ToolUseStart { id, name, summary } => {
            ControllerEvent::ToolUseStart { id, name, summary }
        }
        AgentEvent::PermissionRequest {
            tool_use_id,
            tool_name,
            summary,
            respond,
        } => {
            pending.insert(tool_use_id.clone(), (respond, tool_name.clone()));
            ControllerEvent::PermissionRequest {
                tool_use_id,
                tool_name,
                summary,
            }
        }
        AgentEvent::ToolResult {
            id,
            content,
            is_error,
        } => ControllerEvent::ToolResult {
            id,
            content,
            is_error,
        },
        AgentEvent::TurnComplete => ControllerEvent::TurnComplete,
        AgentEvent::MaxIterations => ControllerEvent::MaxIterations,
        AgentEvent::Notice { text } => ControllerEvent::Notice { text },
        AgentEvent::Error { message } => ControllerEvent::Error { message },
    }
}

// Resolve to the current controller's next UiEvent, or never resolve while
// detached. Pulled out so the `select!` UI branch borrows `attached` only for the
// duration of its own future.
async fn recv_controller_ui(attached: &mut Option<ControllerChannels>) -> Option<UiEvent> {
    match attached {
        Some(c) => c.ui_rx.recv().await,
        None => std::future::pending().await,
    }
}

// Mediate between the loop and at most one attached front-end. Owns the
// session's full event history (an in-memory buffer, replayed on attach) and the
// outstanding permission senders. The loop's channels live here for the whole
// session, so a front-end leaving never closes them — only an explicit Quit (or
// the loop ending on its own) stops the broker.
async fn broker(
    loop_ui_tx: mpsc::Sender<UiEvent>,
    mut loop_agent_rx: mpsc::Receiver<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<HostCommand>,
    seed: Vec<ControllerEvent>,
) {
    let mut attached: Option<ControllerChannels> = None;
    // Pre-seeded with the resumed transcript (if any), so the first attach
    // replays history + live as one stream. Live events append after the seed.
    let mut buffer: Vec<ControllerEvent> = seed;
    let mut pending: HashMap<String, (oneshot::Sender<bool>, String)> = HashMap::new();

    loop {
        tokio::select! {
            cmd = ctl_rx.recv() => {
                match cmd {
                    Some(HostCommand::Attach { ui_rx, event_tx, force, ack }) => {
                        if attached.is_some() && !force {
                            let _ = ack.send(false); // busy — single-attach lock holds
                        } else {
                            // `force` replaces the holder: dropping the old
                            // ControllerChannels closes its event_tx, so the
                            // booted front-end sees its stream end.
                            attached = Some(ControllerChannels { ui_rx, event_tx });
                            let _ = ack.send(true);
                            // Replay the full history before any live event. We're
                            // in this handler synchronously, so loop/controller
                            // branches can't interleave — replayed events strictly
                            // precede live ones.
                            if let Some(c) = attached.as_ref() {
                                for ev in &buffer {
                                    let _ = c.event_tx.send(ev.clone()).await;
                                }
                            }
                        }
                    }
                    Some(HostCommand::Detach) => attached = None,
                    // Buffer + forward a system Notice, exactly like a translated
                    // loop Notice, so it survives into later attach-replays.
                    Some(HostCommand::Quit) | None => {
                        let _ = loop_ui_tx.send(UiEvent::Quit).await;
                        return;
                    }
                }
            }
            ev = loop_agent_rx.recv() => {
                match ev {
                    Some(ev) => {
                        let cev = translate(ev, &mut pending);
                        buffer.push(cev.clone());
                        if let Some(c) = attached.as_ref() {
                            let _ = c.event_tx.send(cev).await;
                        }
                    }
                    None => return, // loop ended on its own
                }
            }
            ui = recv_controller_ui(&mut attached) => {
                match ui {
                    // Answer a permission: fulfil the loop's held oneshot and
                    // record a resolution marker. Never forwarded to the loop.
                    Some(UiEvent::PermissionResponse { tool_use_id, allow }) => {
                        if let Some((tx, tool_name)) = pending.remove(&tool_use_id) {
                            let _ = tx.send(allow);
                            let resolved = ControllerEvent::PermissionResolved { tool_name, allow };
                            buffer.push(resolved.clone());
                            if let Some(c) = attached.as_ref() {
                                let _ = c.event_tx.send(resolved).await;
                            }
                        }
                    }
                    // Drive the loop, and echo into the stream so every controller
                    // (including a later attach) reconstructs the user's turns.
                    Some(UiEvent::UserMessage { text }) => {
                        let _ = loop_ui_tx.send(UiEvent::UserMessage { text: text.clone() }).await;
                        let echo = ControllerEvent::UserMessage { text };
                        buffer.push(echo.clone());
                        if let Some(c) = attached.as_ref() {
                            let _ = c.event_tx.send(echo).await;
                        }
                    }
                    // Pure control (SetModel / MCP load-unload-list / Quit): forward
                    // to the loop; its effects return as events.
                    Some(other) => {
                        let _ = loop_ui_tx.send(other).await;
                    }
                    // Front-end dropped its ui_tx without an explicit detach → the
                    // loop lives on, headless.
                    None => attached = None,
                }
            }
        }
    }
}

// Test-only: stand up a real broker with no agent loop behind it, returning a
// genuine `BrokerHandle` plus the loop-side channel ends. Lets tests in *other*
// modules (notably the socket transport) drive the broker over the real public
// surface — injecting `AgentEvent`s exactly as `run_agent` would — without a fake
// Provider/Backend. Lives at module scope (not inside `mod tests`) so it can
// construct the private `BrokerHandle`/`broker` that those tests can't reach.
#[cfg(test)]
#[allow(dead_code)] // a general fixture: not every consumer touches every field
pub(crate) struct BareBroker {
    pub handle: BrokerHandle,
    // Inject loop events here; they land in the replay buffer and fan out to the
    // attached front-end, just like real loop output.
    pub agent_tx: mpsc::Sender<AgentEvent>,
    // What the broker forwards toward the loop (user messages, model switches, …).
    pub loop_ui_rx: mpsc::Receiver<UiEvent>,
    pub task: JoinHandle<()>,
}

#[cfg(test)]
pub(crate) fn spawn_bare_broker(seed: Vec<ControllerEvent>) -> BareBroker {
    let (loop_ui_tx, loop_ui_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (agent_tx, loop_agent_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, seed));
    BareBroker {
        handle: BrokerHandle { ctl_tx },
        agent_tx,
        loop_ui_rx,
        task,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror SessionHost::attach but allow choosing `force`, so a test can probe
    // both the busy refusal and the local-TUI takeover.
    async fn try_attach(
        ctl_tx: &mpsc::UnboundedSender<HostCommand>,
        force: bool,
    ) -> Option<(mpsc::Receiver<ControllerEvent>, mpsc::Sender<UiEvent>)> {
        let (event_tx, event_rx) = mpsc::channel(16);
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (ack_tx, ack_rx) = oneshot::channel();
        ctl_tx
            .send(HostCommand::Attach {
                ui_rx,
                event_tx,
                force,
                ack: ack_tx,
            })
            .unwrap();
        if ack_rx.await.unwrap_or(false) {
            Some((event_rx, ui_tx))
        } else {
            None
        }
    }

    // User messages are forwarded to the loop AND echoed into the stream; a
    // permission request is translated (oneshot stripped) and the response
    // fulfils the loop's held oneshot, then a resolution marker is emitted.
    #[tokio::test]
    async fn forwards_echoes_and_correlates_permission() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<UiEvent>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a_events, a_ui_tx) = try_attach(&ctl_tx, false).await.expect("first attach");

        // User message: forwarded to the loop and echoed back to the controller.
        a_ui_tx
            .send(UiEvent::UserMessage { text: "hi".into() })
            .await
            .unwrap();
        match loop_ui_rx.recv().await {
            Some(UiEvent::UserMessage { text }) => assert_eq!(text, "hi"),
            other => panic!("expected UserMessage forwarded to loop, got {other:?}"),
        }
        match a_events.recv().await {
            Some(ControllerEvent::UserMessage { text }) => assert_eq!(text, "hi"),
            other => panic!("expected UserMessage echo, got {other:?}"),
        }

        // Permission request from the loop carries a oneshot; the broker strips it.
        let (resp_tx, resp_rx) = oneshot::channel::<bool>();
        loop_agent_tx
            .send(AgentEvent::PermissionRequest {
                tool_use_id: "t1".into(),
                tool_name: "Bash".into(),
                summary: "run ls".into(),
                respond: resp_tx,
            })
            .await
            .unwrap();
        match a_events.recv().await {
            Some(ControllerEvent::PermissionRequest {
                tool_use_id,
                tool_name,
                summary,
            }) => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(tool_name, "Bash");
                assert_eq!(summary, "run ls");
            }
            other => panic!("expected translated PermissionRequest, got {other:?}"),
        }

        // The response fulfils the loop's held oneshot and emits a resolution.
        a_ui_tx
            .send(UiEvent::PermissionResponse {
                tool_use_id: "t1".into(),
                allow: true,
            })
            .await
            .unwrap();
        assert!(resp_rx.await.unwrap());
        match a_events.recv().await {
            Some(ControllerEvent::PermissionResolved { tool_name, allow }) => {
                assert_eq!(tool_name, "Bash");
                assert!(allow);
            }
            other => panic!("expected PermissionResolved, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // Single-attach exclusivity (busy refusal), full replay on (re)attach, and
    // local-TUI force-takeover booting the current holder.
    #[tokio::test]
    async fn single_attach_replay_and_force_takeover() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<UiEvent>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        // A attaches; one event is buffered.
        let (mut a_events, _a_ui_tx) = try_attach(&ctl_tx, false).await.expect("A attach");
        loop_agent_tx
            .send(AgentEvent::AssistantText {
                text: "hello".into(),
            })
            .await
            .unwrap();
        match a_events.recv().await {
            Some(ControllerEvent::AssistantText { text }) => assert_eq!(text, "hello"),
            other => panic!("expected A to receive AssistantText, got {other:?}"),
        }

        // A non-forced attach is refused while A holds the session (single-attach).
        assert!(
            try_attach(&ctl_tx, false).await.is_none(),
            "a non-forced attach must be refused while a controller holds the session"
        );

        // Force-takeover boots A and binds B; B replays the buffered history.
        let (mut b_events, b_ui_tx) = try_attach(&ctl_tx, true).await.expect("B force-attach");
        match b_events.recv().await {
            Some(ControllerEvent::AssistantText { text }) => assert_eq!(text, "hello"),
            other => panic!("expected B replay of AssistantText, got {other:?}"),
        }
        assert!(
            a_events.recv().await.is_none(),
            "force-takeover must close the booted controller's stream"
        );

        // A live event reaches B.
        loop_agent_tx
            .send(AgentEvent::Notice {
                text: "live".into(),
            })
            .await
            .unwrap();
        match b_events.recv().await {
            Some(ControllerEvent::Notice { text }) => assert_eq!(text, "live"),
            other => panic!("expected B to receive live Notice, got {other:?}"),
        }

        // B detaches by dropping; the broker (and the loop's channels) survive —
        // proven by a fresh attach replaying the full buffer. Use force so the
        // test doesn't depend on the broker having yet noticed B's drop (a
        // non-forced attach would race that eventually-consistent detach notice).
        drop(b_events);
        drop(b_ui_tx);
        let (mut c_events, _c_ui_tx) = try_attach(&ctl_tx, true).await.expect("C force-attach");
        match c_events.recv().await {
            Some(ControllerEvent::AssistantText { text }) => assert_eq!(text, "hello"),
            other => panic!("expected C replay [0], got {other:?}"),
        }
        match c_events.recv().await {
            Some(ControllerEvent::Notice { text }) => assert_eq!(text, "live"),
            other => panic!("expected C replay [1], got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // Explicit detach (the /background path) frees the lock so a *non-forced*
    // reattach succeeds, deterministically — Detach and the next Attach ride the
    // same FIFO control channel, so the broker processes the detach first (no
    // race against the eventually-consistent drop-detach). Reattach replays.
    #[tokio::test]
    async fn explicit_detach_frees_lock_for_nonforced_reattach() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<UiEvent>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a_events, _a_ui_tx) = try_attach(&ctl_tx, false).await.expect("A attach");
        loop_agent_tx
            .send(AgentEvent::AssistantText { text: "x".into() })
            .await
            .unwrap();
        match a_events.recv().await {
            Some(ControllerEvent::AssistantText { text }) => assert_eq!(text, "x"),
            other => panic!("expected A to receive AssistantText, got {other:?}"),
        }

        // Explicit detach, then a non-forced reattach — deterministically admitted.
        ctl_tx.send(HostCommand::Detach).unwrap();
        let (mut b_events, _b_ui_tx) = try_attach(&ctl_tx, false)
            .await
            .expect("non-forced reattach must succeed after an explicit detach");
        match b_events.recv().await {
            Some(ControllerEvent::AssistantText { text }) => assert_eq!(text, "x"),
            other => panic!("expected B replay after detach, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }
}
