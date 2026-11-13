use std::collections::{HashMap, VecDeque};
use std::future::Future;

use super::events::{ControllerEvent, UiEvent};
use super::host::{BrokerHandle, Controller, SessionHost};
use super::identity::ClientIdentity;

// Peers this agent is a client of. Each is an ordinary `Controller`, so local and
// remote peers are indistinguishable here; the composition root supplies them.

// Unrelated to the broker's `ControllerId`.
pub type PeerId = u64;

// Supplied by the composition root because building an agent needs things `core` must
// not construct. Gets the spawner's own broker so the child can attach back.
pub type PeerFactory = Box<
    dyn Fn(
            String,
            BrokerHandle,
        )
            -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<PeerRegistration>> + Send>>
        + Send
        + Sync,
>;

// The completed edge returns through `peer_register_rx` so the loop never blocks on
// network I/O.
pub type PeerDialer = Box<
    dyn Fn(
            String,
            BrokerHandle,
        ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
        + Send
        + Sync,
>;

struct Peer {
    who: ClientIdentity,
    controller: Controller,
    // Owns a locally spawned child's tasks, so dropping the Peer ends the child.
    _host: Option<SessionHost>,
    // True = this agent spawned the peer. The return edge stays unsupervised so a
    // child can't answer its parent's gated calls ahead of the human.
    supervised: bool,
    // True = this peer spawned this agent; the loop reports stalls up this edge.
    spawner: bool,
    // Capped and drained per check-in, so supervision cost scales with check-ins,
    // not the peer's verbosity.
    recent: VecDeque<String>,
}

// The child's loop holds its own broker handle, so only an explicit Quit ends it.
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(host) = &self._host {
            host.request_quit();
        }
    }
}

const RECENT_LINES_CAP: usize = 10;
const RECENT_LINE_CHARS_CAP: usize = 120;

pub struct PeerRegistration {
    pub controller: Controller,
    pub who: ClientIdentity,
    pub host: Option<SessionHost>,
    // Set by local wiring or granted by a same-user socket target, never taken from a
    // relay peer, so a remote side can't claim it.
    pub supervised: bool,
    // Set on a child's return edge, or on a socket target's edge back to its supervisor.
    pub spawner: bool,
}

#[derive(Default)]
pub struct PeerWiring {
    pub factory: Option<PeerFactory>,
    pub initial_peers: PeerSet,
    pub register_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PeerRegistration>>,
    pub dialer: Option<PeerDialer>,
}

impl PeerRegistration {
    pub fn new(controller: Controller, who: ClientIdentity) -> Self {
        Self {
            controller,
            who,
            host: None,
            supervised: false,
            spawner: false,
        }
    }
}

#[derive(Default)]
pub struct PeerSet {
    peers: HashMap<PeerId, Peer>,
    next_id: PeerId,
}

impl PeerSet {
    pub fn register(&mut self, reg: PeerRegistration) -> PeerId {
        let id = self.next_id;
        self.next_id += 1;
        self.peers.insert(
            id,
            Peer {
                who: reg.who,
                controller: reg.controller,
                _host: reg.host,
                supervised: reg.supervised,
                spawner: reg.spawner,
                recent: VecDeque::new(),
            },
        );
        id
    }

    pub fn is_supervised(&self, id: PeerId) -> bool {
        self.peers.get(&id).is_some_and(|p| p.supervised)
    }

    pub fn spawner_id(&self) -> Option<PeerId> {
        self.peers
            .iter()
            .find(|(_, p)| p.spawner)
            .map(|(id, _)| *id)
    }

    pub fn has_supervised(&self) -> bool {
        self.peers.values().any(|p| p.supervised)
    }

    // Unsupervised peers are never steered, so their activity isn't buffered.
    pub fn record_activity(&mut self, id: PeerId, line: &str) {
        let Some(p) = self.peers.get_mut(&id) else {
            return;
        };
        if !p.supervised {
            return;
        }
        let clipped = if line.chars().count() > RECENT_LINE_CHARS_CAP {
            let mut c: String = line.chars().take(RECENT_LINE_CHARS_CAP).collect();
            c.push('…');
            c
        } else {
            line.to_string()
        };
        if p.recent.len() == RECENT_LINES_CAP {
            p.recent.pop_front();
        }
        p.recent.push_back(clipped);
    }

    pub fn drain_activity(&mut self, id: PeerId) -> Vec<String> {
        self.peers
            .get_mut(&id)
            .map(|p| p.recent.drain(..).collect())
            .unwrap_or_default()
    }

    // Pends forever when empty (`select_all` panics on an empty iterator). `None` means
    // that peer's stream closed; the caller reaps it with `remove`.
    pub async fn recv(&mut self) -> (PeerId, Option<ControllerEvent>) {
        if self.peers.is_empty() {
            std::future::pending().await
        } else {
            let futures = self.peers.iter_mut().map(|(id, p)| {
                let id = *id;
                Box::pin(async move { (id, p.controller.events.recv().await) })
            });
            let ((id, ev), _idx, _rest) = futures::future::select_all(futures).await;
            (id, ev)
        }
    }

    // A failed send is harmless: the gone peer is reaped on the next `recv`.
    pub async fn drive(&self, id: PeerId, ev: UiEvent) {
        if let Some(p) = self.peers.get(&id) {
            let _ = p.controller.ui_tx.send(ev).await;
        }
    }

    pub fn remove(&mut self, id: PeerId) {
        self.peers.remove(&id);
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    pub fn find_by_name(&self, name: &str) -> Option<PeerId> {
        self.peers
            .iter()
            .find(|(_, p)| p.who.name == name)
            .map(|(id, _)| *id)
    }

    // Sorted so the wording is deterministic.
    pub fn roster(&self) -> Vec<String> {
        let mut names: Vec<String> = self.peers.values().map(|p| p.who.name.clone()).collect();
        names.sort();
        names
    }

    // Sorted so change detection against a prior snapshot is deterministic.
    pub fn infos(&self) -> Vec<super::events::PeerInfo> {
        let mut out: Vec<super::events::PeerInfo> = self
            .peers
            .values()
            .map(|p| super::events::PeerInfo {
                name: p.who.name.clone(),
                kind: p.who.kind.clone(),
                supervised: p.supervised,
                session_id: p.who.session_id.clone(),
                task: p.who.task.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn display_name(&self, id: PeerId) -> String {
        self.peers
            .get(&id)
            .map(|p| p.who.name.clone())
            .unwrap_or_else(|| format!("peer {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    // Returns the far ends a test uses to inject events and observe what's driven.
    fn fake_peer() -> (
        Controller,
        mpsc::UnboundedSender<ControllerEvent>,
        mpsc::Receiver<UiEvent>,
    ) {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = mpsc::channel(16);
        (
            Controller {
                events: ev_rx,
                ui_tx,
            },
            ev_tx,
            ui_rx,
        )
    }

    #[tokio::test]
    async fn recv_merges_and_tags_by_peer() {
        let mut peers = PeerSet::default();
        let (a_ctrl, a_ev, _a_ui) = fake_peer();
        let (b_ctrl, b_ev, _b_ui) = fake_peer();
        let a = peers.register(PeerRegistration::new(
            a_ctrl,
            ClientIdentity::human("alice"),
        ));
        let b = peers.register(PeerRegistration::new(b_ctrl, ClientIdentity::human("bob")));

        b_ev.send(ControllerEvent::Notice {
            text: "from-b".into(),
        })
        .unwrap();
        match peers.recv().await {
            (id, Some(ControllerEvent::Notice { text })) => {
                assert_eq!(id, b);
                assert_eq!(text, "from-b");
            }
            other => panic!("expected b's Notice, got {other:?}"),
        }

        a_ev.send(ControllerEvent::Notice {
            text: "from-a".into(),
        })
        .unwrap();
        match peers.recv().await {
            (id, Some(ControllerEvent::Notice { text })) => {
                assert_eq!(id, a);
                assert_eq!(text, "from-a");
            }
            other => panic!("expected a's Notice, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn drive_reaches_the_addressed_peer() {
        let mut peers = PeerSet::default();
        let (ctrl, _ev, mut ui_rx) = fake_peer();
        let id = peers.register(PeerRegistration::new(ctrl, ClientIdentity::human("child")));

        peers
            .drive(id, UiEvent::UserMessage { text: "go".into() })
            .await;
        match ui_rx.recv().await {
            Some(UiEvent::UserMessage { text }) => assert_eq!(text, "go"),
            other => panic!("expected driven UserMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closed_peer_reports_none_and_reaps_without_disturbing_others() {
        let mut peers = PeerSet::default();
        let (a_ctrl, a_ev, _a_ui) = fake_peer();
        let (b_ctrl, b_ev, _b_ui) = fake_peer();
        let a = peers.register(PeerRegistration::new(
            a_ctrl,
            ClientIdentity::human("alice"),
        ));
        let b = peers.register(PeerRegistration::new(b_ctrl, ClientIdentity::human("bob")));

        drop(a_ev);
        match peers.recv().await {
            (id, None) => {
                assert_eq!(id, a);
                peers.remove(id);
            }
            other => panic!("expected A closed, got {other:?}"),
        }

        b_ev.send(ControllerEvent::Notice {
            text: "still-here".into(),
        })
        .unwrap();
        match peers.recv().await {
            (id, Some(ControllerEvent::Notice { text })) => {
                assert_eq!(id, b);
                assert_eq!(text, "still-here");
            }
            other => panic!("expected B's Notice, got {other:?}"),
        }
    }
}
