use anyhow::Result;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::agent::{AgentConfig, AgentIo, Backend, LoopInput, pre_approves_permissions, run_agent};
use super::events::{AgentEvent, ControllerEvent, UiEvent};
use super::identity::{ClientIdentity, ClientKind};
use super::peer::PeerWiring;
use super::profile::{ClientProfile, CommandScope};
use super::session::Session;
use crate::llm::{Message, Provider};

const CHANNEL_CAPACITY: usize = 64;

// Never exposed on `Controller`: detach is drop-based.
type ControllerId = u64;

// `events` is unbounded so fan-out never blocks on one slow consumer.
pub struct Controller {
    pub events: mpsc::UnboundedReceiver<ControllerEvent>,
    pub ui_tx: mpsc::Sender<UiEvent>,
}

#[derive(Clone, Debug)]
pub enum HandoffStatus {
    Connecting,
    // On the relay; the QR is now scannable.
    Connected,
    Failed(String),
}

pub trait SessionHandle {
    // Every attach is admitted; the stream starts with a full replay. `None` means the
    // transport or broker is gone.
    fn attach(&self, who: ClientIdentity) -> impl Future<Output = Option<Controller>> + Send;
    // Inert seam for a future local-TUI reclaim policy; `tui::run` calls it on foreground.
    fn attach_force(&self, who: ClientIdentity) -> impl Future<Output = Option<Controller>> + Send {
        self.attach(who)
    }
    // Pause-in-place: the loop keeps running headless and buffering.
    fn detach(&self);
}

struct ControllerChannels {
    ui_rx: mpsc::Receiver<UiEvent>,
    event_tx: mpsc::UnboundedSender<ControllerEvent>,
    // Attribution only — never rights.
    who: ClientIdentity,
    // Assigned by provenance at attach, never claimed by the client.
    profile: ClientProfile,
}

enum HostCommand {
    // `ack` fires after the replay is queued, so the caller only gets its `Controller`
    // once history is in flight. It carries the granted profile.
    Attach {
        ui_rx: mpsc::Receiver<UiEvent>,
        event_tx: mpsc::UnboundedSender<ControllerEvent>,
        who: ClientIdentity,
        profile: ClientProfile,
        ack: oneshot::Sender<ClientProfile>,
    },
    // A dialing peer: `supervisor` unless an attached controller already answers
    // permissions, else `agent_peer`. Resolved in the broker so racing dialers can't
    // both win.
    AttachPeer {
        ui_rx: mpsc::Receiver<UiEvent>,
        event_tx: mpsc::UnboundedSender<ControllerEvent>,
        who: ClientIdentity,
        ack: oneshot::Sender<ClientProfile>,
    },
    Quit,
}

// The loop's channels terminate at the broker, which lives for the whole session, so
// front-ends coming and going never close them; only an explicit Quit ends the loop.
pub struct SessionHost {
    agent_task: JoinHandle<Result<()>>,
    broker_task: JoinHandle<()>,
    ctl_tx: mpsc::UnboundedSender<HostCommand>,
    // Injected so `core` never names the transport. The hook dedupes its own dials.
    handoff_hook: Option<Box<dyn Fn() + Send + Sync>>,
}

impl SessionHost {
    // `initial_peers` is seeded before the first poll so a spawned child holds its
    // return edge from its very first turn, with no select! race.
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
        // Created before the loop so the factory can wire a child's return edge to it.
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let self_handle = BrokerHandle {
            ctl_tx: ctl_tx.clone(),
        };
        // Set stickily by the broker on the second distinct driving sender.
        let multi_driver = Arc::new(AtomicBool::new(false));

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
                peer_register_rx: peers.register_rx,
                peer_factory: peers.factory,
                peer_dialer: peers.dialer,
                self_handle,
                multi_driver: multi_driver.clone(),
            },
        ));

        // The resumed transcript as controller events, so a remote client rebuilds
        // history from the stream alone.
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            seed,
            multi_driver,
        ));

        Self {
            agent_task,
            broker_task,
            ctl_tx,
            handoff_hook: None,
        }
    }

    pub fn broker_handle(&self) -> BrokerHandle {
        BrokerHandle {
            ctl_tx: self.ctl_tx.clone(),
        }
    }

    pub fn set_handoff_hook(&mut self, hook: impl Fn() + Send + Sync + 'static) {
        self.handoff_hook = Some(Box::new(hook));
    }

    // Idempotent: if the loop already ended, the send no-ops and the tasks return.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.ctl_tx.send(HostCommand::Quit);
        let loop_result = self.agent_task.await?;
        let _ = self.broker_task.await;
        loop_result
    }

    // Dropping a `SessionHost` only detaches its JoinHandles and the loop holds its own
    // broker handle, so without this a dismissed child would idle forever.
    pub(crate) fn request_quit(&self) {
        let _ = self.ctl_tx.send(HostCommand::Quit);
    }
    // For callers that know the edge's provenance and assign rights directly.
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

    // The real detach is the TUI dropping its `Controller`; this only arms handoff.
    fn detach(&self) {
        if let Some(hook) = &self.handoff_hook {
            hook();
        }
    }
}

#[derive(Clone)]
pub struct BrokerHandle {
    ctl_tx: mpsc::UnboundedSender<HostCommand>,
}

impl BrokerHandle {
    pub fn is_alive(&self) -> bool {
        !self.ctl_tx.is_closed()
    }

    pub async fn closed(&self) {
        self.ctl_tx.closed().await
    }

    // The ui channel stays bounded: backpressure on one flooding sender is wanted.
    async fn request_attach(
        &self,
        command: impl FnOnce(
            mpsc::Receiver<UiEvent>,
            mpsc::UnboundedSender<ControllerEvent>,
            oneshot::Sender<ClientProfile>,
        ) -> HostCommand,
    ) -> Option<(Controller, ClientProfile)> {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.ctl_tx.send(command(ui_rx, event_tx, ack_tx)).is_err() {
            return None;
        }
        let granted = ack_rx.await.ok()?;
        Some((
            Controller {
                events: event_rx,
                ui_tx,
            },
            granted,
        ))
    }

    async fn attach_inner(
        &self,
        who: ClientIdentity,
        profile: ClientProfile,
    ) -> Option<Controller> {
        self.request_attach(|ui_rx, event_tx, ack| HostCommand::Attach {
            ui_rx,
            event_tx,
            who,
            profile,
            ack,
        })
        .await
        .map(|(controller, _)| controller)
    }

    pub async fn attach_as(
        &self,
        who: ClientIdentity,
        profile: ClientProfile,
    ) -> Option<Controller> {
        self.attach_inner(who, profile).await
    }

    // `true` = granted `supervisor`; otherwise `agent_peer`. The broker decides.
    pub async fn attach_peer(&self, who: ClientIdentity) -> Option<(Controller, bool)> {
        let (controller, granted) = self
            .request_attach(|ui_rx, event_tx, ack| HostCommand::AttachPeer {
                ui_rx,
                event_tx,
                who,
                ack,
            })
            .await?;
        Some((controller, granted == ClientProfile::supervisor()))
    }
}

// Derived from the *claimed* kind, so no security decision may depend on it; real
// provenance is assigned via `attach_as`.
fn default_profile(who: &ClientIdentity) -> ClientProfile {
    match who.kind {
        ClientKind::Human => ClientProfile::human(),
        ClientKind::Agent => ClientProfile::agent_peer(),
    }
}

impl SessionHandle for BrokerHandle {
    async fn attach(&self, who: ClientIdentity) -> Option<Controller> {
        let profile = default_profile(&who);
        self.attach_inner(who, profile).await
    }

    // Detach is drop-based: the broker reaps a dropped `Controller` on its next poll.
    fn detach(&self) {}
}

// Terminates the permission `oneshot`: the broker keeps the Sender until the matching
// PermissionResponse arrives.
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
            auto,
        } => ControllerEvent::SessionInfo {
            model,
            cwd,
            git_branch,
            session_id,
            session_name,
            auto,
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

// Dropping Notices for peer agents breaks the mutual-attach amplification cascade.
// Emptying `commands` keeps the advertised menu in sync with what `verb_allowed` accepts.
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

// A pre-approving command (/auto) also needs the right to answer permissions.
fn verb_allowed(profile: &ClientProfile, ev: &UiEvent) -> bool {
    match ev {
        UiEvent::Quit => profile.may_quit,
        UiEvent::UserMessage { .. } => profile.may_drive,
        UiEvent::Command { line } => {
            matches!(profile.commands, CommandScope::All)
                && (profile.may_answer_permissions || !pre_approves_permissions(line))
        }
        _ => true,
    }
}

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

// `None` means that controller dropped its `ui_tx`; the caller reaps it.
async fn recv_any_ui(
    attached: &mut HashMap<ControllerId, ControllerChannels>,
) -> (ControllerId, Option<UiEvent>) {
    if attached.is_empty() {
        // select_all panics on an empty iterator; an attach wakes the ctl_rx arm.
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

async fn broker(
    loop_ui_tx: mpsc::Sender<LoopInput>,
    mut loop_agent_rx: mpsc::Receiver<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<HostCommand>,
    seed: Vec<ControllerEvent>,
    multi_driver: Arc<AtomicBool>,
) {
    let mut attached: HashMap<ControllerId, ControllerChannels> = HashMap::new();
    let mut next_id: ControllerId = 0;
    // Seeded from resumed senders so a past multi-party session stays attributed; the
    // flag is sticky so attribution never flaps when a driver leaves.
    let mut driver_names: std::collections::HashSet<String> = seed
        .iter()
        .filter_map(|ev| match ev {
            ControllerEvent::UserMessage { sender, .. } if !sender.is_empty() => {
                Some(sender.clone())
            }
            _ => None,
        })
        .collect();
    if driver_names.len() >= 2 {
        multi_driver.store(true, Ordering::Relaxed);
    }
    let mut buffer: Vec<ControllerEvent> = seed;
    let mut pending: HashMap<String, (oneshot::Sender<bool>, String)> = HashMap::new();

    loop {
        tokio::select! {
            cmd = ctl_rx.recv() => {
                let (ui_rx, event_tx, who, profile, ack) = match cmd {
                    Some(HostCommand::Attach { ui_rx, event_tx, who, profile, ack }) => {
                        (ui_rx, event_tx, who, profile, ack)
                    }
                    Some(HostCommand::AttachPeer { ui_rx, event_tx, who, ack }) => {
                        // A dropped front-end lingers until reaped; it answers nothing.
                        let answered = attached.values().any(|c| {
                            c.profile.may_answer_permissions && !c.event_tx.is_closed()
                        });
                        let profile = if answered {
                            ClientProfile::agent_peer()
                        } else {
                            ClientProfile::supervisor()
                        };
                        (ui_rx, event_tx, who, profile, ack)
                    }
                    Some(HostCommand::Quit) | None => {
                        let _ = loop_ui_tx.send((None, UiEvent::Quit)).await;
                        return;
                    }
                };
                if profile.may_drive && !who.name.is_empty() {
                    driver_names.insert(who.name.clone());
                    if driver_names.len() >= 2 {
                        multi_driver.store(true, Ordering::Relaxed);
                    }
                }
                // No await in this handler, so replay strictly precedes live
                // events for this controller.
                for ev in &buffer {
                    if let Some(projected) = project(&profile, ev) {
                        let _ = event_tx.send(projected);
                    }
                }
                let id = next_id;
                next_id += 1;
                attached.insert(id, ControllerChannels { ui_rx, event_tx, who, profile: profile.clone() });
                let _ = ack.send(profile);
            }
            ev = loop_agent_rx.recv() => {
                match ev {
                    Some(ev) => {
                        let cev = translate(ev, &mut pending);
                        buffer.push(cev.clone());
                        fan_out(&mut attached, &cev);
                    }
                    None => return,
                }
            }
            (id, ui) = recv_any_ui(&mut attached) => {
                match ui {
                    // First responder wins; a later answer finds nothing pending. The
                    // profile gate stops a child rubber-stamping its parent's gate.
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
                    // The sender identity is stamped here from the registry, never
                    // claimed by the client.
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
                    // Refusals are sent directly, bypassing `project`, which would
                    // withhold a Notice from an agent.
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
                    None => { attached.remove(&id); }
                }
            }
        }
    }
}

// A real broker with no agent loop, so other modules' tests can inject `AgentEvent`s
// without a fake Provider/Backend. Module scope so it can reach private items.
#[cfg(test)]
#[allow(dead_code)] // a general fixture: not every consumer touches every field
pub(crate) struct BareBroker {
    pub handle: BrokerHandle,
    pub agent_tx: mpsc::Sender<AgentEvent>,
    pub loop_ui_rx: mpsc::Receiver<LoopInput>,
    pub task: JoinHandle<()>,
}

#[cfg(test)]
pub(crate) fn spawn_bare_broker(seed: Vec<ControllerEvent>) -> BareBroker {
    let (loop_ui_tx, loop_ui_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (agent_tx, loop_agent_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(broker(
        loop_ui_tx,
        loop_agent_rx,
        ctl_rx,
        seed,
        Arc::new(AtomicBool::new(false)),
    ));
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

    async fn try_attach(
        ctl_tx: &mpsc::UnboundedSender<HostCommand>,
    ) -> (
        mpsc::UnboundedReceiver<ControllerEvent>,
        mpsc::Sender<UiEvent>,
    ) {
        try_attach_as(ctl_tx, "test").await
    }

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
        assert!(ack_rx.await.is_ok(), "attach must be admitted");
        (event_rx, ui_tx)
    }

    async fn expect_text(events: &mut mpsc::UnboundedReceiver<ControllerEvent>, want: &str) {
        match events.recv().await {
            Some(ControllerEvent::AssistantText { text }) if text == want => {}
            other => panic!("expected AssistantText({want:?}), got {other:?}"),
        }
    }

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
        assert!(ack_rx.await.is_ok(), "attach must be admitted");
        (event_rx, ui_tx)
    }

    // Same kind, different rights — the distinction ClientKind can't make.
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
        assert!(ack_rx.await.is_ok(), "attach must be admitted");
        (event_rx, ui_tx)
    }

    // The trailing AssistantText proves the agent's stream skipped the Notice, not stalled.
    #[tokio::test]
    async fn notices_are_withheld_from_agent_controllers() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

        match human.recv().await {
            Some(ControllerEvent::Notice { text }) => assert_eq!(text, "peer did a thing"),
            other => panic!("human expected Notice, got {other:?}"),
        }
        expect_text(&mut human, "next").await;
        expect_text(&mut agent, "next").await;

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    #[tokio::test]
    async fn quit_is_gated_to_human_clients() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        let (mut agent, a_ui) = try_attach_agent(&ctl_tx).await;
        let (_human, h_ui) = try_attach(&ctl_tx).await;

        a_ui.send(UiEvent::Quit).await.unwrap();
        match agent.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("not permitted")),
            other => panic!("agent expected refusal Notice, got {other:?}"),
        }

        h_ui.send(UiEvent::Quit).await.unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::Quit)) => {}
            other => panic!("loop expected human Quit, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    // Otherwise a child could rubber-stamp its parent's gate under first-responder-wins.
    #[tokio::test]
    async fn agent_peer_cannot_answer_permissions_but_human_can() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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
        match human.recv().await {
            Some(ControllerEvent::PermissionRequest { tool_use_id, .. }) => {
                assert_eq!(tool_use_id, "t1")
            }
            other => panic!("human expected PermissionRequest, got {other:?}"),
        }
        let _ = agent.recv().await;

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

    #[tokio::test]
    async fn supervisor_can_answer_permissions() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

    // Proven on replay from the seed buffer.
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
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            seed,
            Arc::new(AtomicBool::new(false)),
        ));

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

    #[tokio::test]
    async fn commands_are_gated_by_profile() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        let (mut agent, a_ui) = try_attach_agent(&ctl_tx).await;
        let (_human, h_ui) = try_attach(&ctl_tx).await;

        a_ui.send(UiEvent::Command {
            line: "/model x".into(),
        })
        .await
        .unwrap();
        match agent.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("not permitted")),
            other => panic!("agent expected refusal Notice, got {other:?}"),
        }

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

    // /auto pre-approves Spawn prompts, so it needs more than command rights.
    #[tokio::test]
    async fn auto_needs_the_right_to_answer_permissions() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        let (mut peer, p_ui) = try_attach_with(&ctl_tx, ClientProfile::agent()).await;
        let (_human, h_ui) = try_attach(&ctl_tx).await;

        p_ui.send(UiEvent::Command {
            line: "/auto".into(),
        })
        .await
        .unwrap();
        match peer.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("not permitted")),
            other => panic!("peer expected refusal Notice, got {other:?}"),
        }
        p_ui.send(UiEvent::Command {
            line: "/peers".into(),
        })
        .await
        .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::Command { line })) => assert_eq!(line, "/peers"),
            other => panic!("loop expected the peer's /peers, got {other:?}"),
        }

        h_ui.send(UiEvent::Command {
            line: "/auto".into(),
        })
        .await
        .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::Command { line })) => assert_eq!(line, "/auto"),
            other => panic!("loop expected human /auto, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    #[tokio::test]
    async fn watch_only_cannot_send_messages_but_still_observes() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        let (mut watcher, w_ui) = try_attach_with(&ctl_tx, ClientProfile::watch_only()).await;
        let (_driver, d_ui) = try_attach(&ctl_tx).await;

        w_ui.send(UiEvent::UserMessage {
            text: "let me steer".into(),
        })
        .await
        .unwrap();
        match watcher.recv().await {
            Some(ControllerEvent::Notice { text }) => assert!(text.contains("watch-only")),
            other => panic!("watcher expected refusal Notice, got {other:?}"),
        }

        d_ui.send(UiEvent::UserMessage { text: "go".into() })
            .await
            .unwrap();
        match loop_ui_rx.recv().await {
            Some((_, UiEvent::UserMessage { text })) => assert_eq!(text, "go"),
            other => panic!("loop expected the driver's message, got {other:?}"),
        }
        // The refused message was never echoed: the next event is the driver's turn.
        match watcher.recv().await {
            Some(ControllerEvent::UserMessage { text, .. }) => assert_eq!(text, "go"),
            other => panic!("watcher expected to observe the driver's turn, got {other:?}"),
        }

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    #[tokio::test]
    async fn forwards_echoes_and_correlates_permission() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        let (mut a_events, a_ui_tx) = try_attach(&ctl_tx).await;

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

    #[tokio::test]
    async fn fan_out_reaches_all_controllers() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

    #[tokio::test]
    async fn either_controller_drives_all_see_echo() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

    #[tokio::test]
    async fn user_message_echo_is_attributed_to_sender() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        let (mut a, a_ui) = try_attach_as(&ctl_tx, "alice").await;
        let (mut b, _b_ui) = try_attach_as(&ctl_tx, "bob").await;

        a_ui.send(UiEvent::UserMessage { text: "hi".into() })
            .await
            .unwrap();
        match loop_ui_rx.recv().await {
            Some((who, UiEvent::UserMessage { text })) => {
                assert_eq!(text, "hi");
                assert_eq!(who.expect("loop forward carries the sender").name, "alice");
            }
            other => panic!("expected UserMessage forwarded to loop, got {other:?}"),
        }
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

    #[tokio::test]
    async fn permission_first_responder_wins() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

        // B's late answer must no-op: the next event each sees is fresh, not a
        // duplicate PermissionResolved.
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

    #[tokio::test]
    async fn dropping_one_controller_keeps_the_rest() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

    #[tokio::test]
    async fn slow_controller_does_not_block_others_or_the_loop() {
        let (loop_ui_tx, mut loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

        // Bound but never drained.
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

    #[tokio::test]
    async fn late_attach_replays_with_others_present() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
        ));

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

        let (mut b, _b_ui) = try_attach(&ctl_tx).await;
        expect_text(&mut b, "one").await;
        expect_text(&mut b, "two").await;

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

    #[tokio::test]
    async fn multi_driver_flips_on_second_distinct_driving_attach() {
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let multi = Arc::new(AtomicBool::new(false));
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            Vec::new(),
            multi.clone(),
        ));

        let (_a, _a_ui) = try_attach_as(&ctl_tx, "alice").await;
        assert!(!multi.load(Ordering::Relaxed), "one driver → solo");

        let (_w, _w_ui) = try_attach_with(&ctl_tx, ClientProfile::watch_only()).await;
        let (_a2, _a2_ui) = try_attach_as(&ctl_tx, "alice").await;
        assert!(
            !multi.load(Ordering::Relaxed),
            "watch-only and same-name attaches don't make a session multi-driver"
        );

        let (_b, _b_ui) = try_attach_as(&ctl_tx, "bob").await;
        assert!(
            multi.load(Ordering::Relaxed),
            "a second distinct driving sender flips the flag"
        );

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    #[tokio::test]
    async fn multi_driver_seeds_from_replay_senders() {
        let seed = vec![
            ControllerEvent::UserMessage {
                text: "hi".into(),
                sender: "alice".into(),
            },
            ControllerEvent::UserMessage {
                text: "yo".into(),
                sender: "bob".into(),
            },
        ];
        let (loop_ui_tx, _loop_ui_rx) = mpsc::channel::<LoopInput>(8);
        let (_loop_agent_tx, loop_agent_rx) = mpsc::channel::<AgentEvent>(8);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<HostCommand>();
        let multi = Arc::new(AtomicBool::new(false));
        let broker_task = tokio::spawn(broker(
            loop_ui_tx,
            loop_agent_rx,
            ctl_rx,
            seed,
            multi.clone(),
        ));

        // The flag is set before the select loop; one attach round-trip proves it ran.
        let (_a, _a_ui) = try_attach(&ctl_tx).await;
        assert!(
            multi.load(Ordering::Relaxed),
            "two distinct persisted senders → multi from the start"
        );

        ctl_tx.send(HostCommand::Quit).unwrap();
        broker_task.await.unwrap();
    }

    fn agent(name: &str) -> ClientIdentity {
        ClientIdentity {
            kind: ClientKind::Agent,
            name: name.into(),
            session_id: None,
            task: None,
        }
    }

    #[tokio::test]
    async fn a_peer_with_a_human_attached_is_not_its_supervisor() {
        let bb = spawn_bare_broker(Vec::new());
        let _human = bb.handle.attach(ClientIdentity::human("h")).await.unwrap();
        let (_peer, supervised) = bb.handle.attach_peer(agent("a")).await.unwrap();
        assert!(!supervised);
        bb.task.abort();
    }

    #[tokio::test]
    async fn only_the_first_peer_of_an_unanswered_session_supervises() {
        let bb = spawn_bare_broker(Vec::new());
        let (_first, first) = bb.handle.attach_peer(agent("a")).await.unwrap();
        let (_second, second) = bb.handle.attach_peer(agent("b")).await.unwrap();
        assert!(first && !second);
        bb.task.abort();
    }

    #[tokio::test]
    async fn a_departed_answerer_leaves_supervision_open() {
        let bb = spawn_bare_broker(Vec::new());
        drop(bb.handle.attach(ClientIdentity::human("h")).await.unwrap());
        let (_peer, supervised) = bb.handle.attach_peer(agent("a")).await.unwrap();
        assert!(supervised);
        bb.task.abort();
    }

    #[tokio::test]
    async fn racing_peers_yield_exactly_one_supervisor() {
        let bb = spawn_bare_broker(Vec::new());
        let attaches = (0..8).map(|i| {
            let handle = bb.handle.clone();
            tokio::spawn(async move { handle.attach_peer(agent(&format!("p{i}"))).await })
        });
        let mut controllers = Vec::new();
        for task in attaches.collect::<Vec<_>>() {
            controllers.push(task.await.unwrap().unwrap());
        }
        assert_eq!(controllers.iter().filter(|(_, s)| *s).count(), 1);
        bb.task.abort();
    }
}
