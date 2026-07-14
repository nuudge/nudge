use std::collections::{HashMap, VecDeque};
use std::future::Future;

use super::events::{ControllerEvent, UiEvent};
use super::host::{BrokerHandle, Controller, SessionHost};
use super::identity::ClientIdentity;

// The set of peers this agent is a *client* of — the outbound half of symmetric
// communication. Each peer is held as an ordinary `Controller` (the same one a
// human front-end gets from `attach`), so `observe` = drain its events and `drive`
// = send on its `ui_tx`. This module names no transport: a local peer's controller
// comes from `SessionHost::attach`, a remote peer's from `RelayClient::attach`, and
// this set can't tell them apart — the composition root creates them and hands them
// in via `register`.

// Local routing key for one held peer. Broker-side controllers have their own
// `ControllerId`; this is the mirror on the client side and is unrelated to it.
pub type PeerId = u64;

// Creates a peer on demand — the executor behind the model-facing `Spawn` tool. The
// composition root supplies it (building an agent needs a provider, a backend, and a
// session — all things `core` must not construct), and the loop calls it with the
// task plus a handle to its OWN broker so the factory can wire the return edge
// (child attaches back to the spawner). Absent for agents that may not spawn (a
// factory-made child gets `None`, so there is no recursive spawning yet).
pub type PeerFactory = Box<
    dyn Fn(
            String,
            BrokerHandle,
        )
            -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<PeerRegistration>> + Send>>
        + Send
        + Sync,
>;

// A peer this agent holds a client connection to.
struct Peer {
    who: ClientIdentity,
    controller: Controller,
    // Keep-alive for a locally spawned child: the `SessionHost` owns the child's loop
    // and broker tasks, so dropping this `Peer` (reap, or the parent ending) ends the
    // child. `None` when the peer's lifetime is owned elsewhere (the return edge held
    // by a child, a remote peer, a test's synthetic controller).
    _host: Option<SessionHost>,
    // Direction of creation: true = this agent spawned the peer, so it steers the
    // peer's permission check-ins and may dismiss it. The return edge a child holds
    // to its spawner is unsupervised — which is what stops a child answering its
    // parent's gated calls (first-responder-wins would let it beat the human).
    supervised: bool,
    // Compact activity digest for the NEXT steering check-in (supervised peers
    // only): a capped ring of one-line records, drained when a check-in is composed
    // and folded nowhere else — supervision cost scales with check-ins, not with
    // the peer's verbosity.
    recent: VecDeque<String>,
}

// The most a steering check-in may carry from the activity ring: these caps are the
// context-frugality contract (see supervision_plan.md decision 2).
const RECENT_LINES_CAP: usize = 10;
const RECENT_LINE_CHARS_CAP: usize = 120;

// A peer handed to the loop at runtime (spawned by the factory, or registered by the
// composition root). Carries the controller plus the peer's announced identity, so
// the loop can attribute the peer's activity to a name.
pub struct PeerRegistration {
    pub controller: Controller,
    pub who: ClientIdentity,
    pub host: Option<SessionHost>,
    // Set only by this agent's own Spawn path (trust-by-wiring — never announced
    // over the handshake, so a peer cannot claim it).
    pub supervised: bool,
}

// The peer capabilities an agent is born with: whether it may spawn subagents (the
// factory) and which peers it already holds — e.g. a spawned child starts with its
// return edge to the spawner seeded here, so MessagePeer is available from its very
// first turn. Default = a plain session with neither.
#[derive(Default)]
pub struct PeerWiring {
    pub factory: Option<PeerFactory>,
    pub initial_peers: PeerSet,
}

impl PeerRegistration {
    // A registration with no owned host and no supervision — for peers whose
    // lifetime and steering live elsewhere (a return edge, a test controller).
    pub fn new(controller: Controller, who: ClientIdentity) -> Self {
        Self {
            controller,
            who,
            host: None,
            supervised: false,
        }
    }
}

// Holds every peer the agent drives/observes and merges their event streams into
// one input for the loop. Empty by default, so a top-level session (no peers) never
// exercises any of this.
#[derive(Default)]
pub struct PeerSet {
    peers: HashMap<PeerId, Peer>,
    next_id: PeerId,
}

impl PeerSet {
    // Start holding a peer; returns its local id (the routing key for `drive`).
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
                recent: VecDeque::new(),
            },
        );
        id
    }

    pub fn is_supervised(&self, id: PeerId) -> bool {
        self.peers.get(&id).is_some_and(|p| p.supervised)
    }

    // Whether any held peer is supervised — gates the RespondToPeer/DismissPeer
    // schemas (an agent that supervises nobody has no verdicts to deliver).
    pub fn has_supervised(&self) -> bool {
        self.peers.values().any(|p| p.supervised)
    }

    // Record one line of observed activity for a supervised peer's next check-in.
    // No-op for unsupervised peers — this agent will never steer them, so buffering
    // their activity would pay for context nobody spends.
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

    // Take the activity recorded since the last check-in (clears the ring).
    pub fn drain_activity(&mut self, id: PeerId) -> Vec<String> {
        self.peers
            .get_mut(&id)
            .map(|p| p.recent.drain(..).collect())
            .unwrap_or_default()
    }

    // Resolve to the next event from ANY held peer, tagged by its id, or never
    // resolve while none are held (so the loop's other `select!` arms keep running
    // — `select_all` panics on an empty iterator, so guard it). `None` = that peer's
    // event stream closed (it went away); the caller reaps it with `remove`. Same
    // borrow discipline as the broker's `recv_any_ui`: the losing futures are dropped
    // before returning, ending the `&mut` borrow, and the handler re-borrows fresh.
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

    // Drive a peer: send it a UiEvent up its `ui_tx` (an instruction, or a permission
    // verdict). Best-effort — a peer whose channel is gone is reaped on the next
    // `recv`, so a failed send here is a harmless no-op.
    pub async fn drive(&self, id: PeerId, ev: UiEvent) {
        if let Some(p) = self.peers.get(&id) {
            let _ = p.controller.ui_tx.send(ev).await;
        }
    }

    // Stop holding a peer (its stream closed). No-op if already gone.
    pub fn remove(&mut self, id: PeerId) {
        self.peers.remove(&id);
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    // Resolve a peer by its announced name — how the model addresses one.
    pub fn find_by_name(&self, name: &str) -> Option<PeerId> {
        self.peers
            .iter()
            .find(|(_, p)| p.who.name == name)
            .map(|(id, _)| *id)
    }

    // The names of every held peer, for the model's error recovery ("no such peer;
    // current peers: …"). Sorted so the wording is deterministic.
    pub fn roster(&self) -> Vec<String> {
        let mut names: Vec<String> = self.peers.values().map(|p| p.who.name.clone()).collect();
        names.sort();
        names
    }

    // Display name for a peer, for attributing its activity in Notices. Falls back to
    // the routing id if the peer is already gone.
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

    // A synthetic peer: the `Controller` the PeerSet holds, plus the far ends a test
    // uses to inject the peer's events and observe what the PeerSet drives to it.
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

    // recv() merges every held peer's stream and tags each event with the right id.
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

    // drive() sends a UiEvent up the addressed peer's ui_tx.
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

    // A peer whose event stream closes surfaces as (id, None); removing it leaves the
    // other peers intact and still mergeable.
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

        drop(a_ev); // A's broker/stream is gone
        match peers.recv().await {
            (id, None) => {
                assert_eq!(id, a);
                peers.remove(id);
            }
            other => panic!("expected A closed, got {other:?}"),
        }

        // B is unaffected: its events still merge after A was reaped.
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
