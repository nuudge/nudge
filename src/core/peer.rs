use std::collections::{HashMap, VecDeque};
use std::future::Future;

use super::events::{ControllerEvent, UiEvent};
use super::host::{BrokerHandle, Controller, SessionHost};
use super::identity::ClientIdentity;
use super::profile::ClientProfile;

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

    // The two halves of a mutual peer edge between A and B. If A supervises B, A's edge
    // to B is `supervised` and B's edge back to A is its `spawner`, so B reports stalls
    // up it; otherwise both are plain. B attaches back with `back_profile`.
    pub fn forward(controller: Controller, who: ClientIdentity, supervised: bool) -> Self {
        Self {
            supervised,
            ..Self::new(controller, who)
        }
    }

    pub fn back(controller: Controller, who: ClientIdentity, supervised: bool) -> Self {
        Self {
            spawner: supervised,
            ..Self::new(controller, who)
        }
    }

    // A supervised B gets no command scope over its supervisor; a plain peer keeps the
    // `/connect-peer` grant. Neither can answer A's prompts.
    pub fn back_profile(supervised: bool) -> ClientProfile {
        if supervised {
            ClientProfile::agent_peer()
        } else {
            ClientProfile::agent()
        }
    }
}

#[derive(Default)]
pub struct PeerSet {
    peers: HashMap<PeerId, Peer>,
    next_id: PeerId,
}

// Each carries the id of the edge that survived.
#[derive(Debug, PartialEq)]
pub enum Registered {
    Added(PeerId),
    Replaced(PeerId),
    Rejected(PeerId),
}

impl PeerSet {
    // One edge per peer session: a supervised or spawner edge replaces a plain one, and
    // anything else is dropped, so a reconnect never downgrades an edge.
    pub fn register(&mut self, reg: PeerRegistration) -> Registered {
        let existing = reg.who.session_id.as_ref().and_then(|sid| {
            self.peers
                .iter()
                .find(|(_, p)| p.who.session_id.as_ref() == Some(sid))
                .map(|(id, p)| (*id, p.supervised || p.spawner))
        });
        match existing {
            None => Registered::Added(self.insert(reg)),
            Some((old, false)) if reg.supervised || reg.spawner => {
                self.peers.remove(&old);
                Registered::Replaced(self.insert(reg))
            }
            Some((old, _)) => Registered::Rejected(old),
        }
    }

    fn insert(&mut self, reg: PeerRegistration) -> PeerId {
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

    pub fn role(&self, id: PeerId) -> &'static str {
        match self.peers.get(&id) {
            Some(p) if p.supervised => "supervised",
            Some(p) if p.spawner => "supervising",
            _ => "unsupervised",
        }
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
        let Registered::Added(a) = peers.register(PeerRegistration::new(
            a_ctrl,
            ClientIdentity::human("alice"),
        )) else {
            unreachable!()
        };
        let Registered::Added(b) =
            peers.register(PeerRegistration::new(b_ctrl, ClientIdentity::human("bob")))
        else {
            unreachable!()
        };

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
        let Registered::Added(id) =
            peers.register(PeerRegistration::new(ctrl, ClientIdentity::human("child")))
        else {
            unreachable!()
        };

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
        let Registered::Added(a) = peers.register(PeerRegistration::new(
            a_ctrl,
            ClientIdentity::human("alice"),
        )) else {
            unreachable!()
        };
        let Registered::Added(b) =
            peers.register(PeerRegistration::new(b_ctrl, ClientIdentity::human("bob")))
        else {
            unreachable!()
        };

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

    fn edge(
        session_id: Option<&str>,
        supervised: bool,
    ) -> (PeerRegistration, mpsc::Receiver<UiEvent>) {
        let (ctrl, _ev, ui_rx) = fake_peer();
        let who = ClientIdentity {
            session_id: session_id.map(String::from),
            ..ClientIdentity::human("x")
        };
        (PeerRegistration::forward(ctrl, who, supervised), ui_rx)
    }

    #[tokio::test]
    async fn a_stronger_edge_to_the_same_session_replaces_the_plain_one() {
        let mut peers = PeerSet::default();
        let (plain, mut plain_ui) = edge(Some("s"), false);
        let (strong, _strong_ui) = edge(Some("s"), true);
        assert_eq!(peers.register(plain), Registered::Added(0));
        assert_eq!(peers.register(strong), Registered::Replaced(1));
        assert_eq!(peers.infos().len(), 1);
        assert!(peers.is_supervised(1));
        assert!(
            plain_ui.recv().await.is_none(),
            "the old edge's controller must drop"
        );
    }

    #[tokio::test]
    async fn a_weaker_or_equal_edge_to_the_same_session_is_rejected() {
        let mut peers = PeerSet::default();
        let (strong, _strong_ui) = edge(Some("s"), true);
        let (plain, mut plain_ui) = edge(Some("s"), false);
        let (again, _again_ui) = edge(Some("s"), true);
        assert_eq!(peers.register(strong), Registered::Added(0));
        assert_eq!(peers.register(plain), Registered::Rejected(0));
        assert_eq!(peers.register(again), Registered::Rejected(0));
        assert_eq!(peers.infos().len(), 1);
        assert!(peers.is_supervised(0));
        assert!(
            plain_ui.recv().await.is_none(),
            "the new edge's controller must drop"
        );
    }

    #[test]
    fn edges_without_a_session_id_are_never_deduplicated() {
        let mut peers = PeerSet::default();
        let (a, _a_ui) = edge(None, false);
        let (b, _b_ui) = edge(None, true);
        assert_eq!(peers.register(a), Registered::Added(0));
        assert_eq!(peers.register(b), Registered::Added(1));
        assert_eq!(peers.infos().len(), 2);
    }
}
