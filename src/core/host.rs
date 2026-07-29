use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::agent::{AgentConfig, AgentIo, Backend, LoopInput, run_agent};
use super::events::{AgentEvent, ControllerEvent, UiEvent};
use super::identity::{ClientIdentity, ClientKind};
use super::peer::PeerWiring;
use super::profile::{ClientProfile, CommandScope};
use super::session::Session;
use crate::llm::{Message, Provider};

const CHANNEL_CAPACITY: usize = 64;

// The broker's registry key for one attached controller. Broker-internal: it keys
// fan-out and reaping, and is never exposed on `Controller` (detach is drop-based).
type ControllerId = u64;

// A front-end's handle to the live session: it receives the controller event
// stream and sends UiEvents back. The front-end owns these ends; the broker owns
// the matching ends and mediates between them and the loop. The event stream is
// unbounded so the broker can fan an event to every controller without one slow
// consumer blocking delivery to the others or to the loop (head-of-line blocking).
pub struct Controller {
    pub events: mpsc::UnboundedReceiver<ControllerEvent>,
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
    // and the UiEvent sink). `who` is the attaching party's identity, announced at
    // the handshake and recorded by the broker. Many front-ends can attach at once
    // (a human plus a watcher, a peer agent, …); each gets its own replay + live
    // stream. `None` = the transport or broker is gone. The future is `Send` so a
    // server can drive it from a spawned task.
    fn attach(&self, who: ClientIdentity) -> impl Future<Output = Option<Controller>> + Send;
    // Retained seam for a local-TUI reclaim policy. Currently inert — with no
    // single-attach lock there is nothing to force, so the default (a plain
    // `attach`, admitting a second controller alongside the first) is the whole
    // behaviour. `tui::run` still calls it on every foreground. A future
    // designated-driver policy would key off handshake identity and live here.
    fn attach_force(&self, who: ClientIdentity) -> impl Future<Output = Option<Controller>> + Send {
        self.attach(who)
    }
    // Yield the front-end without ending the session (pause-in-place, e.g.
    // /background). Fire-and-forget: the loop keeps running headless and buffering;
    // a later `attach` replays the full history.
    fn detach(&self);
}

// The broker's view of one attached controller.
struct ControllerChannels {
    ui_rx: mpsc::Receiver<UiEvent>,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    // The attaching party's announced identity. Recorded at attach and read when the
    // broker stamps this controller's messages, so a shared session attributes each
    // turn to a named party. Attribution only — never rights (see `profile`).
    who: ClientIdentity,
    // The attaching party's rights, assigned by provenance at attach (never claimed by
    // the client). Read by the broker's routing (`deliver_to`) and verb gating
    // (`verb_allowed`); the loop never sees it.
    profile: ClientProfile,
}

// Control messages from `SessionHost`'s methods to the broker task.
enum HostCommand {
    // Bind a front-end. The broker always admits it (there is no single-attach
    // lock); `ack` fires `true` once the controller is registered and its replay
    // has been queued, so the caller only sees its `Controller` after history is
    // in flight. A dropped `ack` (or `false`, unused) means the broker is gone.
    Attach {
        ui_rx: mpsc::Receiver<UiEvent>,
        event_tx: mpsc::UnboundedSender<ControllerEvent>,
        who: ClientIdentity,
        profile: ClientProfile,
        ack: oneshot::Sender<bool>,
    },
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
    // design — see the Provider/Backend trait docs). `peers` is what the agent is
    // born with: `factory` = the executor behind the model-facing Spawn tool (None =
    // this agent may not spawn — the factory itself creates children with None, so
    // spawning doesn't recurse); `initial_peers` seeds the loop's PeerSet before its
    // first poll, so a spawned child holds its return edge — and the MessagePeer
    // tool — from its very first turn (no select! race between kick and
    // registration).
    pub fn spawn<P, B>(
        cfg: AgentConfig,
        provider: P,
        backend: B,
        session: Session,
        initial_messages: Vec<Message>,
        seed: Vec<ControllerEvent>,
        peers: PeerWiring,
    ) -> Self
    where
        // Sync because the loop lends `&provider` / `&backend` to the steering turn,
        // which holds them across its own awaits (session logging, escalation).
        P: Provider + Send + Sync + 'static,
        B: Backend + Send + Sync + 'static,
    {
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (loop_ui_tx, loop_ui_rx) = mpsc::channel(CHANNEL_CAPACITY);
        // Created before the loop so it can carry a handle to its OWN broker — the
        // factory needs it to wire a spawned child's return edge (child → parent).
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let self_handle = BrokerHandle {
            ctl_tx: ctl_tx.clone(),
        };

        let agent_task = tokio::spawn(run_agent(
            cfg,
            provider,
            backend,
            session,
            initial_messages,
            AgentIo {
                ui_rx: loop_ui_rx,
                agent_tx: loop_agent_tx,
                peers: peers.initial_peers,
                // No runtime registrar wired yet — peers arrive at spawn (this seed)
                // or via the loop's own Spawn tool. The AgentIo seam stays for a
                // future producer (e.g. remotely-connected peers).
                peer_register_rx: None,
                peer_factory: peers.factory,
                self_handle,
            },
        ));

        // `seed` is the resumed transcript pre-translated to controller events
        // (empty for a fresh session). It primes the broker's replay buffer so
        // every controller — including a remote one that can't read the JSONL —
        // reconstructs the full history from the event stream alone.
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
    // Attach with an explicit profile, assigned by whoever knows the provenance of the
    // edge (e.g. `spawn.rs` gives a spawned child's supervisor edge a supervisor
    // profile). `SessionHandle::attach` is the identity-only door that derives an
    // interim profile from `who.kind`; this is the door for callers that assign rights
    // directly.
    pub async fn attach_as(
        &self,
        who: ClientIdentity,
        profile: ClientProfile,
    ) -> Option<Controller> {
        self.broker_handle().attach_as(who, profile).await
    }
}

impl SessionHandle for SessionHost {
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        self.broker_handle().attach(who).await
    }

    // The actual detach happens when the TUI drops its `Controller` (run_loop sets
    // both channel slots to None on /background); this call only arms handoff by
    // firing the injected hook (it dials the relay). The hook no-ops while a dial is
    // already live, so firing every time just lets a failed dial be retried by
    // backgrounding again.
    fn detach(&self) {
        if let Some(hook) = &self.handoff_hook {
            hook();
        }
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
    // relay dial loop uses this after a controller's stream closes: broker alive →
    // keep dialing so a device can attach again; broker gone (host shutdown) → stop.
    pub fn is_alive(&self) -> bool {
        !self.ctl_tx.is_closed()
    }

    // Shared body of attach / attach_force / attach_as. The event stream is unbounded
    // so the broker's fan-out never blocks on this controller; the ui channel stays
    // bounded (this direction is one controller → the broker, so local backpressure
    // on a flooding sender is the right behaviour). `who` is the attaching party's
    // identity and `profile` its rights, both recorded by the broker.
    async fn attach_inner(
        &self,
        who: ClientIdentity,
        profile: ClientProfile,
    ) -> Option<Controller> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .ctl_tx
            .send(HostCommand::Attach {
                ui_rx,
                event_tx,
                who,
                profile,
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
            _ => None, // broker gone
        }
    }

    // Attach with an explicit profile — the door for callers that assign rights by
    // provenance rather than deriving them from the claimable `who.kind`.
    pub async fn attach_as(
        &self,
        who: ClientIdentity,
        profile: ClientProfile,
    ) -> Option<Controller> {
        self.attach_inner(who, profile).await
    }
}

// The interim rights a client gets when it comes in through the identity-only
// `SessionHandle::attach` door (a local TUI, or a remote client whose Attach frame the
// daemon relays). Derived from the *claimed* `kind` — a known interim: it is a default,
// never the thing a security decision depends on. Real provenance (a spawned edge's
// direction; later, a pairing's baked-in scope) is assigned via `attach_as`.
fn default_profile(who: &ClientIdentity) -> ClientProfile {
    match who.kind {
        ClientKind::Human => ClientProfile::human(),
        ClientKind::Agent => ClientProfile::agent_peer(),
    }
}

impl SessionHandle for BrokerHandle {
    // Always admits (no single-attach lock); `None` only if the broker is gone.
    // The controller's event stream begins with a replay of the full history, then
    // live events. `attach_force` uses the trait default (== `attach`).
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        let profile = default_profile(&who);
        self.attach_inner(who, profile).await
    }

    // Detach is drop-based: a controller leaves by dropping its `Controller`, whose
    // closed `ui_rx` / `event_tx` the broker reaps on the next poll. With no lock to
    // release, this call is a no-op — kept only to satisfy the trait.
    fn detach(&self) {}
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
            session_name,
        } => ControllerEvent::SessionInfo {
            model,
            cwd,
            git_branch,
            session_id,
            session_name,
        },
        AgentEvent::Capabilities {
            commands,
            models,
            mcp,
            peers,
        } => ControllerEvent::Capabilities {
            commands,
            models,
            mcp,
            peers,
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

// Adapt an event to a controller's profile before delivery. Supervision chatter
// (`Notice`) is dropped for a client that doesn't receive it (a peer agent) — the
// boundary that breaks the mutual-attach amplification cascade, so the loop can narrate
// peer activity without a special case. A `Capabilities` event has its command list
// emptied for a client with no command rights, so the menu a client is shown ("what I'm
// told I can do") matches the verbs the broker will accept from it ("what I may do") and
// the two cannot drift; `models`/`mcp` stay — they're render info, only `commands` is
// rights-bearing. `None` = drop the event entirely.
fn project(profile: &ClientProfile, ev: &ControllerEvent) -> Option<ControllerEvent> {
    match ev {
        ControllerEvent::Notice { .. } if !profile.receives_supervision => None,
        ControllerEvent::Capabilities {
            models, mcp, peers, ..
        } if profile.commands == CommandScope::None => Some(ControllerEvent::Capabilities {
            commands: Vec::new(),
            models: models.clone(),
            mcp: mcp.clone(),
            peers: peers.clone(),
        }),
        _ => Some(ev.clone()),
    }
}

// Broker-boundary authority policy: whether a controller with this profile may issue a
// verb. Kept minimal — only the verbs that exist today. `UserMessage` drives a turn,
// `Command` runs a slash-command, `Quit` ends the session; a client without the right
// (a watch-only spectator, a peer agent) is refused. `commands` is a scope so a future
// partly-restricted client is one `Only([...])` away.
fn verb_allowed(profile: &ClientProfile, ev: &UiEvent) -> bool {
    match ev {
        UiEvent::Quit => profile.may_quit,
        UiEvent::UserMessage { .. } => profile.may_drive,
        UiEvent::Command { .. } => matches!(profile.commands, CommandScope::All),
        _ => true,
    }
}

// Send an event to every attached controller, projected through each one's profile
// (supervision Notices dropped, Capabilities command-lists emptied), reaping any whose
// receiver has dropped (an unbounded send only fails when the `Controller.events` end is
// gone). The event channel is unbounded, so this never blocks the broker on a slow
// consumer — one stalled controller can't hold up delivery to the others or the loop.
fn fan_out(attached: &mut HashMap<ControllerId, ControllerChannels>, ev: &ControllerEvent) {
    let mut dead: Vec<ControllerId> = Vec::new();
    for (id, c) in attached.iter() {
        let Some(projected) = project(&c.profile, ev) else {
            continue;
        };
        if c.event_tx.send(projected).is_err() {
            dead.push(*id);
        }
    }
    for id in dead {
        attached.remove(&id);
    }
}

// Resolve to the next UiEvent from ANY attached controller, tagged by its id, or
// never resolve while none are attached (so the broker's other `select!` arms keep
// running). `None` = that controller dropped its `ui_tx` → the caller reaps it.
// Pulled out so the `select!` UI branch borrows `attached` only for the duration of
// its own future; the losing futures are dropped before returning, ending the
// borrow, and the handler body re-borrows fresh.
async fn recv_any_ui(
    attached: &mut HashMap<ControllerId, ControllerChannels>,
) -> (ControllerId, Option<UiEvent>) {
    if attached.is_empty() {
        // select_all panics on an empty iterator, so stay pending until an attach
        // wakes the ctl_rx arm and repopulates the map.
        std::future::pending().await
    } else {
        let futures = attached.iter_mut().map(|(id, c)| {
            let id = *id;
            Box::pin(async move { (id, c.ui_rx.recv().await) })
        });
        let ((id, ev), _idx, _rest) = futures::future::select_all(futures).await;
        (id, ev)
    }
}

// Mediate between the loop and any number of attached front-ends. Owns the
// session's full event history (an in-memory buffer, replayed on attach) and the
// outstanding permission senders. Loop events fan out to every controller; inbound
// UiEvents from every controller merge into the loop's single input. The loop's
// channels live here for the whole session, so a front-end leaving never closes
// them — only an explicit Quit (or the loop ending on its own) stops the broker.
async fn broker(
    loop_ui_tx: mpsc::Sender<LoopInput>,
    mut loop_agent_rx: mpsc::Receiver<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<HostCommand>,
    seed: Vec<ControllerEvent>,
) {
    let mut attached: HashMap<ControllerId, ControllerChannels> = HashMap::new();
    let mut next_id: ControllerId = 0;
    // Pre-seeded with the resumed transcript (if any), so the first attach
    // replays history + live as one stream. Live events append after the seed.
    let mut buffer: Vec<ControllerEvent> = seed;
    let mut pending: HashMap<String, (oneshot::Sender<bool>, String)> = HashMap::new();

    loop {
        tokio::select! {
            cmd = ctl_rx.recv() => {
                match cmd {
                    Some(HostCommand::Attach { ui_rx, event_tx, who, profile, ack }) => {
                        // Replay the full history to this controller before any live
                        // event, projected through its profile just like live fan-out
                        // (so its replay drops supervision Notices and empties the
                        // Capabilities command list identically). This handler has no
                        // await point, so no other arm can interleave — replayed events
                        // strictly precede live ones for this controller. Unbounded send
                        // only errors if the controller already vanished (harmless —
                        // reaped on the next poll / fan-out).
                        for ev in &buffer {
                            if let Some(projected) = project(&profile, ev) {
                                let _ = event_tx.send(projected);
                            }
                        }
                        let id = next_id;
                        next_id += 1;
                        attached.insert(id, ControllerChannels { ui_rx, event_tx, who, profile });
                        let _ = ack.send(true);
                    }
                    // End the session: forward a final Quit to the loop and stop.
                    Some(HostCommand::Quit) | None => {
                        let _ = loop_ui_tx.send((None, UiEvent::Quit)).await;
                        return;
                    }
                }
            }
            ev = loop_agent_rx.recv() => {
                match ev {
                    Some(ev) => {
                        let cev = translate(ev, &mut pending);
                        buffer.push(cev.clone());
                        fan_out(&mut attached, &cev);
                    }
                    None => return, // loop ended on its own
                }
            }
            (id, ui) = recv_any_ui(&mut attached) => {
                match ui {
                    // Answer a permission: fulfil the loop's held oneshot and record
                    // a resolution marker, fanned to all controllers so every UI
                    // clears its prompt. First responder wins — a second answer for
                    // the same id finds nothing in `pending` and no-ops. Never
                    // forwarded to the loop. Gated by profile: a client that may not
                    // answer (a bare peer agent — e.g. a child over its return edge) is
                    // ignored with a direct Notice back to just it, so it cannot
                    // rubber-stamp a gate via first-responder-wins.
                    Some(UiEvent::PermissionResponse { tool_use_id, allow }) => {
                        let may_answer = attached
                            .get(&id)
                            .map(|c| c.profile.may_answer_permissions)
                            .unwrap_or(false);
                        if !may_answer {
                            if let Some(c) = attached.get(&id) {
                                let _ = c.event_tx.send(ControllerEvent::Notice {
                                    text: "answering permission prompts is not permitted for this client".into(),
                                });
                            }
                        } else if let Some((tx, tool_name)) = pending.remove(&tool_use_id) {
                            let _ = tx.send(allow);
                            let resolved = ControllerEvent::PermissionResolved { tool_name, allow };
                            buffer.push(resolved.clone());
                            fan_out(&mut attached, &resolved);
                        }
                    }
                    // Drive the loop, and echo into the stream so every controller
                    // (including a later attach) reconstructs the shared transcript.
                    // Both the loop forward and the echo carry the sending
                    // controller's registry identity — stamped here, never claimed by
                    // the client — so the loop can attribute a peer's message and the
                    // shared transcript shows who said what. Gated by profile.may_drive:
                    // a watch-only client is refused with a direct Notice, and its
                    // message is neither forwarded to the loop nor echoed to anyone.
                    Some(UiEvent::UserMessage { text }) => {
                        let allowed = attached
                            .get(&id)
                            .map(|c| verb_allowed(&c.profile, &UiEvent::UserMessage { text: text.clone() }))
                            .unwrap_or(false);
                        if !allowed {
                            if let Some(c) = attached.get(&id) {
                                let _ = c.event_tx.send(ControllerEvent::Notice {
                                    text: "this client is watch-only and cannot send messages".into(),
                                });
                            }
                        } else {
                            let who = attached.get(&id).map(|c| c.who.clone());
                            let sender = who.as_ref().map(|w| w.name.clone()).unwrap_or_default();
                            let _ = loop_ui_tx
                                .send((who, UiEvent::UserMessage { text: text.clone() }))
                                .await;
                            let echo = ControllerEvent::UserMessage { text, sender };
                            buffer.push(echo.clone());
                            fan_out(&mut attached, &echo);
                        }
                    }
                    // End the session — gated by broker policy (profile.may_quit). A
                    // refused Quit gets a direct Notice back to just that controller
                    // (bypassing the fan-out policy, which would otherwise withhold a
                    // Notice from an agent), so the sender learns why nothing happened.
                    Some(UiEvent::Quit) => {
                        let allowed = attached
                            .get(&id)
                            .map(|c| verb_allowed(&c.profile, &UiEvent::Quit))
                            .unwrap_or(false);
                        if allowed {
                            let who = attached.get(&id).map(|c| c.who.clone());
                            let _ = loop_ui_tx.send((who, UiEvent::Quit)).await;
                        } else if let Some(c) = attached.get(&id) {
                            let _ = c.event_tx.send(ControllerEvent::Notice {
                                text: "quit is not permitted for agent clients".into(),
                            });
                        }
                    }
                    // A slash-command line — gated by profile.commands. Allowed: forward
                    // to the loop; its effects return as Notice / SessionInfo /
                    // Capabilities events. Refused: a direct Notice back to just this
                    // controller (same bypass as the refused Quit).
                    Some(UiEvent::Command { line }) => {
                        let allowed = attached
                            .get(&id)
                            .map(|c| verb_allowed(&c.profile, &UiEvent::Command { line: line.clone() }))
                            .unwrap_or(false);
                        if allowed {
                            let who = attached.get(&id).map(|c| c.who.clone());
                            let _ = loop_ui_tx.send((who, UiEvent::Command { line })).await;
                        } else if let Some(c) = attached.get(&id) {
                            let _ = c.event_tx.send(ControllerEvent::Notice {
                                text: "commands are not permitted for this client".into(),
                            });
                        }
                    }
                    // This controller dropped its ui_tx (detach / quit) → reap it; the
                    // loop and any other controllers live on.
                    None => { attached.remove(&id); }
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
    // What the broker forwards toward the loop (user messages, model switches, …),
    // each stamped with the sending controller's identity.
    pub loop_ui_rx: mpsc::Receiver<LoopInput>,
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

    // Attach a fresh controller to the broker, returning its (unbounded) event
    // stream and its UiEvent sink. Always admitted now — there is no single-attach
    // lock — so the ack is asserted rather than returned.
    async fn try_attach(
        ctl_tx: &mpsc::UnboundedSender<HostCommand>,
    ) -> (
        mpsc::UnboundedReceiver<ControllerEvent>,
        mpsc::Sender<UiEvent>,
    ) {
        try_attach_as(ctl_tx, "test").await
    }

    // Attach with a chosen display name, so attribution tests can tell senders apart.
    async fn try_attach_as(
        ctl_tx: &mpsc::UnboundedSender<HostCommand>,
        name: &str,
    ) -> (
        mpsc::UnboundedReceiver<ControllerEvent>,
        mpsc::Sender<UiEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (ack_tx, ack_rx) = oneshot::channel();
        let who = ClientIdentity::human(name);
        let profile = default_profile(&who);
        ctl_tx
            .send(HostCommand::Attach {
                ui_rx,
                event_tx,
                who,
                profile,
                ack: ack_tx,
            })
            .unwrap();
        assert!(ack_rx.await.unwrap_or(false), "attach must be admitted");
        (event_rx, ui_tx)
    }

    async fn expect_text(events: &mut mpsc::UnboundedReceiver<ControllerEvent>, want: &str) {
        match events.recv().await {
            Some(ControllerEvent::AssistantText { text }) if text == want => {}
            other => panic!("expected AssistantText({want:?}), got {other:?}"),
        }
    }

    // Attach an agent-kind controller, so policy tests can distinguish it from a human.
    async fn try_attach_agent(
        ctl_tx: &mpsc::UnboundedSender<HostCommand>,
    ) -> (
        mpsc::UnboundedReceiver<ControllerEvent>,
        mpsc::Sender<UiEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (ack_tx, ack_rx) = oneshot::channel();
        let who = ClientIdentity {
            kind: ClientKind::Agent,
            name: "peer".into(),
            session_id: None,
            task: None,
        };
        let profile = default_profile(&who);
        ctl_tx
            .send(HostCommand::Attach {
                ui_rx,
                event_tx,
                who,
                profile,
                ack: ack_tx,
            })
            .unwrap();
        assert!(ack_rx.await.unwrap_or(false), "attach must be admitted");
        (event_rx, ui_tx)
    }

    // Attach an agent-kind controller with an explicit profile — for the permission
    // gate tests, which need a bare peer vs. a supervisor edge (same kind, different
    // rights, exactly the distinction ClientKind can't make).
    async fn try_attach_with(
        ctl_tx: &mpsc::UnboundedSender<HostCommand>,
        profile: ClientProfile,
    ) -> (
        mpsc::UnboundedReceiver<ControllerEvent>,
        mpsc::Sender<UiEvent>,
    ) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        let (ack_tx, ack_rx) = oneshot::channel();
        let who = ClientIdentity {
            kind: ClientKind::Agent,
            name: "peer".into(),
            session_id: None,
            task: None,
        };
        ctl_tx
            .send(HostCommand::Attach {
                ui_rx,
                event_tx,
                who,
                profile,
                ack: ack_tx,
            })
            .unwrap();
        assert!(ack_rx.await.unwrap_or(false), "attach must be admitted");
        (event_rx, ui_tx)
    }

    // Supervision chatter (Notice) is withheld from an agent-kind controller but still
    // reaches a human — this is what lets the loop narrate peer activity without the
    // old re-narration hack. A following AssistantText proves the agent's stream simply
    // skipped the Notice (didn't stall).
    #[tokio::test]
    async fn notices_are_withheld_from_agent_controllers() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut human, _h_ui) = try_attach(&ctl_tx).await;
        let (mut agent, _a_ui) = try_attach_agent(&ctl_tx).await;

        loop_agent_tx
            .send(AgentEvent::Notice {
                text: "peer did a thing".into(),
            })
            .await
            .unwrap();
        loop_agent_tx
            .send(AgentEvent::AssistantText {
                text: "next".into(),
            })
            .await
            .unwrap();

        // Human sees the Notice, then the text.
        match human.recv().await {
            Some(ControllerEvent::Notice { text }) => assert_eq!(text, "peer did a thing"),
            other => panic!("human expected Notice, got {other:?}"),
        }
        expect_text(&mut human, "next").await;
        // Agent's stream skipped the Notice entirely — the text is its first event.
        expect_text(&mut agent, "next").await;

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // Quit is gated at the broker boundary: a human's Quit reaches the loop; an agent's
    // is refused with a direct Notice back to it, and never forwarded.
    #[tokio::test]
    async fn quit_is_gated_to_human_clients() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut agent, a_ui) = try_attach_agent(&ctl_tx).await;
        let (_human, h_ui) = try_attach(&ctl_tx).await;

        // Agent's Quit is refused: it gets a Notice, the loop gets nothing.
        a_ui.send(UiEvent::Quit).await.unwrap();
        match agent.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("not permitted")),
            other => panic!("agent expected refusal Notice, got {other:?}"),
        }

        // Human's Quit is forwarded to the loop.
        h_ui.send(UiEvent::Quit).await.unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::Quit)) => {}
            other => panic!("loop expected human Quit, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A bare peer agent (agent_peer profile, may_answer_permissions=false) cannot answer
    // a permission prompt: its PermissionResponse doesn't fulfil the loop's oneshot and
    // it gets a refusal Notice; a human's subsequent answer still wins. Closes the
    // first-responder-wins hole where a child could rubber-stamp its parent's gate.
    #[tokio::test]
    async fn agent_peer_cannot_answer_permissions_but_human_can() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut agent, a_ui) = try_attach_with(&ctl_tx, ClientProfile::agent_peer()).await;
        let (mut human, h_ui) = try_attach(&ctl_tx).await;

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
        // The human sees the prompt; the agent does not answer-gate on seeing it.
        match human.recv().await {
            Some(ControllerEvent::PermissionRequest { tool_use_id, .. }) => {
                assert_eq!(tool_use_id, "t1")
            }
            other => panic!("human expected PermissionRequest, got {other:?}"),
        }
        let _ = agent.recv().await; // drain the agent's copy of the prompt

        // The agent tries to approve: refused with a Notice, oneshot NOT fulfilled.
        a_ui.send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: true,
        })
        .await
        .unwrap();
        match agent.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("not permitted")),
            other => panic!("agent expected refusal Notice, got {other:?}"),
        }

        // The human's answer wins: the oneshot resolves and a resolution is fanned out.
        h_ui.send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: true,
        })
        .await
        .unwrap();
        assert!(
            resp_rx.await.unwrap(),
            "human's answer must fulfil the oneshot"
        );

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // The supervisor edge (agent kind, but may_answer_permissions=true) IS honored — this
    // is the parent steering its child's gated calls (steering.rs drives the verdict down
    // this edge), which must keep working alongside the gate above.
    #[tokio::test]
    async fn supervisor_can_answer_permissions() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut supervisor, s_ui) = try_attach_with(&ctl_tx, ClientProfile::supervisor()).await;

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
        match supervisor.recv().await {
            Some(ControllerEvent::PermissionRequest { .. }) => {}
            other => panic!("supervisor expected PermissionRequest, got {other:?}"),
        }

        s_ui.send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: true,
        })
        .await
        .unwrap();
        assert!(
            resp_rx.await.unwrap(),
            "supervisor's answer must fulfil the oneshot"
        );

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // The Capabilities command list is filtered per profile: a client with no command
    // rights (a peer agent) sees empty `commands` while a human sees the full list;
    // render info (models/mcp) is kept for both. Proven on replay from the seed buffer.
    #[tokio::test]
    async fn capabilities_command_list_is_filtered_by_profile() {
        use crate::core::events::{CommandInfo, ModelInfo};

        let seed = vec![ControllerEvent::Capabilities {
            commands: vec![CommandInfo {
                name: "/model".into(),
                usage: "u".into(),
            }],
            models: vec![ModelInfo {
                id: "id1".into(),
                label: "L1".into(),
            }],
            mcp: Vec::new(),
            peers: Vec::new(),
        }];
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, seed));

        let (mut human, _h_ui) = try_attach(&ctl_tx).await;
        let (mut agent, _a_ui) = try_attach_agent(&ctl_tx).await;

        match human.recv().await {
            Some(ControllerEvent::Capabilities {
                commands, models, ..
            }) => {
                assert_eq!(commands.len(), 1, "human keeps the full command list");
                assert_eq!(models.len(), 1);
            }
            other => panic!("human expected Capabilities, got {other:?}"),
        }
        match agent.recv().await {
            Some(ControllerEvent::Capabilities {
                commands, models, ..
            }) => {
                assert!(commands.is_empty(), "agent's command list is emptied");
                assert_eq!(models.len(), 1, "render info (models) is kept");
            }
            other => panic!("agent expected Capabilities, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A slash-command from a client without command rights (a peer agent) is refused
    // with a Notice and never reaches the loop; a human's command is forwarded.
    #[tokio::test]
    async fn commands_are_gated_by_profile() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut agent, a_ui) = try_attach_agent(&ctl_tx).await;
        let (_human, h_ui) = try_attach(&ctl_tx).await;

        // Agent's command is refused with a Notice; the loop sees nothing.
        a_ui.send(UiEvent::Command {
            line: "/model x".into(),
        })
        .await
        .unwrap();
        match agent.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("not permitted")),
            other => panic!("agent expected refusal Notice, got {other:?}"),
        }

        // Human's command is forwarded to the loop.
        h_ui.send(UiEvent::Command {
            line: "/mcp".into(),
        })
        .await
        .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::Command { line })) => assert_eq!(line, "/mcp"),
            other => panic!("loop expected human Command, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A watch-only client cannot drive: its UserMessage is refused with a Notice, never
    // forwarded to the loop and never echoed to anyone. Its observe stream still receives
    // another (driving) client's turn — watch-only means spectate, not silence.
    #[tokio::test]
    async fn watch_only_cannot_send_messages_but_still_observes() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut watcher, w_ui) = try_attach_with(&ctl_tx, ClientProfile::watch_only()).await;
        let (_driver, d_ui) = try_attach(&ctl_tx).await;

        // The watcher tries to drive: refused with a Notice, nothing forwarded.
        w_ui.send(UiEvent::UserMessage {
            text: "let me steer".into(),
        })
        .await
        .unwrap();
        match watcher.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("watch-only")),
            other => panic!("watcher expected refusal Notice, got {other:?}"),
        }

        // A real driver's message IS forwarded and echoed — and the watcher observes it.
        d_ui.send(UiEvent::UserMessage { text: "go".into() })
            .await
            .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::UserMessage { text })) => assert_eq!(text, "go"),
            other => panic!("loop expected the driver's message, got {other:?}"),
        }
        // The next thing the watcher sees is the driver's echoed turn, not its own
        // (refused) message — proving observe survives while drive is gated.
        match watcher.recv().await {
            Some(ControllerEvent::UserMessage { text, .. }) => assert_eq!(text, "go"),
            other => panic!("watcher expected to observe the driver's turn, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // User messages are forwarded to the loop AND echoed into the stream; a
    // permission request is translated (oneshot stripped) and the response
    // fulfils the loop's held oneshot, then a resolution marker is emitted.
    #[tokio::test]
    async fn forwards_echoes_and_correlates_permission() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a_events, a_ui_tx) = try_attach(&ctl_tx).await;

        // User message: forwarded to the loop and echoed back to the controller.
        a_ui_tx
            .send(UiEvent::UserMessage { text: "hi".into() })
            .await
            .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::UserMessage { text })) => assert_eq!(text, "hi"),
            other => panic!("expected UserMessage forwarded to loop, got {other:?}"),
        }
        match a_events.recv().await {
            Some(ControllerEvent::UserMessage { text, .. }) => assert_eq!(text, "hi"),
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

    // One loop event fans out to every attached controller.
    #[tokio::test]
    async fn fan_out_reaches_all_controllers() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a, _a_ui) = try_attach(&ctl_tx).await;
        let (mut b, _b_ui) = try_attach(&ctl_tx).await;

        loop_agent_tx
            .send(AgentEvent::AssistantText { text: "hi".into() })
            .await
            .unwrap();
        expect_text(&mut a, "hi").await;
        expect_text(&mut b, "hi").await;

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // Either controller can drive the loop; the loop sees the message once, and
    // every controller sees the echo (a shared transcript).
    #[tokio::test]
    async fn either_controller_drives_all_see_echo() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a, a_ui) = try_attach(&ctl_tx).await;
        let (mut b, _b_ui) = try_attach(&ctl_tx).await;

        a_ui.send(UiEvent::UserMessage { text: "go".into() })
            .await
            .unwrap();

        match loop_ui_rx.recv().await {
            Some((_, UiEvent::UserMessage { text })) => assert_eq!(text, "go"),
            other => panic!("expected UserMessage forwarded to loop, got {other:?}"),
        }
        for (who, ev) in [("A", a.recv().await), ("B", b.recv().await)] {
            match ev {
                Some(ControllerEvent::UserMessage { text, .. }) => assert_eq!(text, "go"),
                other => panic!("{who} expected UserMessage echo, got {other:?}"),
            }
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A user message's echo is attributed to the sending controller's identity, and
    // fans to every controller (so a shared session shows who said what).
    #[tokio::test]
    async fn user_message_echo_is_attributed_to_sender() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a, a_ui) = try_attach_as(&ctl_tx, "alice").await;
        let (mut b, _b_ui) = try_attach_as(&ctl_tx, "bob").await;

        a_ui.send(UiEvent::UserMessage { text: "hi".into() })
            .await
            .unwrap();
        // Forwarded to the loop once, stamped with the sender's registry identity —
        // so the loop can attribute a peer's message in the transcript.
        match loop_ui_rx.recv().await {
            Some((who, UiEvent::UserMessage { text })) => {
                assert_eq!(text, "hi");
                assert_eq!(who.expect("loop forward carries the sender").name, "alice");
            }
            other => panic!("expected UserMessage forwarded to loop, got {other:?}"),
        }
        // Both controllers see the echo stamped with alice.
        for (who, ev) in [("A", a.recv().await), ("B", b.recv().await)] {
            match ev {
                Some(ControllerEvent::UserMessage { text, sender }) => {
                    assert_eq!(text, "hi");
                    assert_eq!(sender, "alice");
                }
                other => panic!("{who} expected attributed echo, got {other:?}"),
            }
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A permission request fans to every controller; the first answer fulfils the
    // loop's one-shot and clears the prompt on all. A second answer for the same id
    // finds nothing pending and no-ops (proven: the next event both controllers see
    // is a fresh one, not a duplicate PermissionResolved).
    #[tokio::test]
    async fn permission_first_responder_wins() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a, a_ui) = try_attach(&ctl_tx).await;
        let (mut b, b_ui) = try_attach(&ctl_tx).await;

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
        for (who, ev) in [("A", a.recv().await), ("B", b.recv().await)] {
            match ev {
                Some(ControllerEvent::PermissionRequest { tool_use_id, .. }) => {
                    assert_eq!(tool_use_id, "t1")
                }
                other => panic!("{who} expected PermissionRequest, got {other:?}"),
            }
        }

        // A answers first: the loop's one-shot resolves, both see PermissionResolved.
        a_ui.send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: true,
        })
        .await
        .unwrap();
        assert!(resp_rx.await.unwrap());
        for (who, ev) in [("A", a.recv().await), ("B", b.recv().await)] {
            match ev {
                Some(ControllerEvent::PermissionResolved { allow, .. }) => assert!(allow),
                other => panic!("{who} expected PermissionResolved, got {other:?}"),
            }
        }

        // B answers the same id late — no-op. A fresh loop event is the next thing
        // each controller sees, regardless of the order the broker processes the two.
        b_ui.send(UiEvent::PermissionResponse {
            tool_use_id: "t1".into(),
            allow: false,
        })
        .await
        .unwrap();
        loop_agent_tx
            .send(AgentEvent::AssistantText {
                text: "next".into(),
            })
            .await
            .unwrap();
        expect_text(&mut a, "next").await;
        expect_text(&mut b, "next").await;

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // Dropping one controller reaps it without disturbing the others or the loop.
    #[tokio::test]
    async fn dropping_one_controller_keeps_the_rest() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (a, a_ui) = try_attach(&ctl_tx).await;
        let (mut b, _b_ui) = try_attach(&ctl_tx).await;

        drop(a);
        drop(a_ui);

        loop_agent_tx
            .send(AgentEvent::Notice {
                text: "still here".into(),
            })
            .await
            .unwrap();
        match b.recv().await {
            Some(ControllerEvent::Notice { text }) => assert_eq!(text, "still here"),
            other => panic!("B expected live Notice after A dropped, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A controller that never drains its event stream must not stall delivery to a
    // fast controller or the loop — the unbounded fan-out guarantee.
    #[tokio::test]
    async fn slow_controller_does_not_block_others_or_the_loop() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        // A attaches but never drains (leading underscore = a live, undrained binding).
        let (_a_slow, _a_ui) = try_attach(&ctl_tx).await;
        let (mut b, b_ui) = try_attach(&ctl_tx).await;

        for i in 0..100 {
            loop_agent_tx
                .send(AgentEvent::AssistantText {
                    text: i.to_string(),
                })
                .await
                .unwrap();
        }
        for i in 0..100 {
            expect_text(&mut b, &i.to_string()).await;
        }

        // The loop still receives input driven while A is stuck.
        b_ui.send(UiEvent::UserMessage {
            text: "drive".into(),
        })
        .await
        .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::UserMessage { text })) => assert_eq!(text, "drive"),
            other => panic!("loop expected driven UserMessage, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // A controller attaching after events exist replays the full buffer even while
    // another controller is already attached; then a live event reaches both.
    #[tokio::test]
    async fn late_attach_replays_with_others_present() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(loop_ui_tx, loop_agent_rx, ctl_rx, Vec::new()));

        let (mut a, _a_ui) = try_attach(&ctl_tx).await;
        loop_agent_tx
            .send(AgentEvent::AssistantText { text: "one".into() })
            .await
            .unwrap();
        loop_agent_tx
            .send(AgentEvent::AssistantText { text: "two".into() })
            .await
            .unwrap();
        expect_text(&mut a, "one").await;
        expect_text(&mut a, "two").await;

        // B attaches late — full replay even though A is still attached.
        let (mut b, _b_ui) = try_attach(&ctl_tx).await;
        expect_text(&mut b, "one").await;
        expect_text(&mut b, "two").await;

        // A live event reaches both.
        loop_agent_tx
            .send(AgentEvent::Notice {
                text: "live".into(),
            })
            .await
            .unwrap();
        for (who, ev) in [("A", a.recv().await), ("B", b.recv().await)] {
            match ev {
                Some(ControllerEvent::Notice { text }) => assert_eq!(text, "live"),
                other => panic!("{who} expected live Notice, got {other:?}"),
            }
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }
}
