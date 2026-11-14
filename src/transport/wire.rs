use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::time::{Instant, Interval, interval_at, timeout_at};
use tokio_tungstenite::tungstenite::Message;

use super::encryption::Cipher;
use crate::core::{ClientIdentity, ControllerEvent, UiEvent};

// Newline-delimited JSON over byte streams: compact `serde_json` output escapes every
// newline, so a bare `\n` is an unambiguous frame terminator.

#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ClientFrame {
    // The handshake. `after_seq` is the resume cursor: replay only `seq > after_seq`;
    // `None` replays from 0. `reverse_offer` requests the duplex peer edge, and is
    // skipped when false so ordinary clients' bytes (and the Kotlin mirror) are unaffected.
    Attach {
        after_seq: Option<u64>,
        who: ClientIdentity,
        #[serde(default, skip_serializing_if = "is_false")]
        reverse_offer: bool,
    },
    // Yield the session without ending it; the loop keeps running headless.
    Detach,
    Command(UiEvent),
    // The dialer's own broker events over a reverse-offered edge; `seq` is that
    // direction's replay cursor.
    ReverseEvent {
        seq: u64,
        event: ControllerEvent,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ServerFrame {
    Attached,
    // The session is gone (broker shut down); no `Event` frames will follow.
    Busy,
    // `seq` counts every event the session ever emitted (replay and live share one
    // sequence), so it is a stable resume cursor across reconnects.
    Event {
        seq: u64,
        event: ControllerEvent,
    },
    // Sent once after `Attached`, only if the dialer offered a reverse edge.
    // `supervised` = the dialer was granted `supervisor` over this session.
    ReverseAttach {
        who: ClientIdentity,
        #[serde(default, skip_serializing_if = "is_false")]
        supervised: bool,
    },
    // Never sent to a non-offering client.
    ReverseCommand(UiEvent),
}

// Flushes per frame to keep the interactive control channel low-latency.
pub async fn write_frame<W, T>(w: &mut W, frame: &T) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_vec(frame)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

// `Ok(None)` is a clean EOF, distinct from a transport fault.
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncBufReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    let n = r.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let frame = serde_json::from_str(line.trim_end())?;
    Ok(Some(frame))
}

// Frame-level seam so the same session logic drives either codec: newline-framed
// for a Unix byte stream, one-frame-per-message for a WebSocket.
pub trait FrameReader<F>: Send {
    // `Ok(None)` = clean EOF (peer closed); `Err` = transport or parse fault.
    fn recv(&mut self) -> impl Future<Output = Result<Option<F>>> + Send;
}

pub trait FrameWriter<F>: Send {
    fn send(&mut self, frame: &F) -> impl Future<Output = Result<()>> + Send;

    // Only a relayed WebSocket can die silently (a sleeping laptop leaves it
    // half-open); a Unix socket's peer dying surfaces as EOF.
    fn keepalive(&mut self) -> impl Future<Output = Result<()>> + Send {
        async { Ok(()) }
    }
}

// Matches the phone (OkHttp `pingInterval(20s)`).
const PING_INTERVAL: Duration = Duration::from_secs(20);

// Three ping intervals without even a pong means the socket is dead. Must stay
// above the relay's own ping interval too.
const READ_DEADLINE: Duration = Duration::from_secs(60);

pub fn keepalive_timer() -> Interval {
    interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL)
}

pub struct LineReader<R>(pub R);
pub struct LineWriter<W>(pub W);

impl<R, F> FrameReader<F> for LineReader<R>
where
    R: AsyncBufReadExt + Unpin + Send,
    F: DeserializeOwned,
{
    async fn recv(&mut self) -> Result<Option<F>> {
        read_frame(&mut self.0).await
    }
}

impl<W, F> FrameWriter<F> for LineWriter<W>
where
    W: AsyncWriteExt + Unpin + Send,
    F: Serialize + Sync,
{
    async fn send(&mut self, frame: &F) -> Result<()> {
        write_frame(&mut self.0, frame).await
    }
}

// One sealed frame per WS Binary message, so the relay only forwards ciphertext.
//
// The read deadline is reader state (time of the last frame) rather than a per-call
// timeout, because callers `select!` on `recv` alongside ping ticks and events: each
// of those restarts the call, and a per-call timeout would never trip.
pub struct WsReader<St> {
    stream: St,
    cipher: Cipher,
    deadline: Duration,
    last_frame: Instant,
}
pub struct WsWriter<Si> {
    sink: Si,
    cipher: Cipher,
}

impl<St> WsReader<St> {
    pub fn new(stream: St, cipher: Cipher) -> Self {
        Self {
            stream,
            cipher,
            deadline: READ_DEADLINE,
            last_frame: Instant::now(),
        }
    }
}

impl<Si> WsWriter<Si> {
    pub fn new(sink: Si, cipher: Cipher) -> Self {
        Self { sink, cipher }
    }
}

impl<St, E, F> FrameReader<F> for WsReader<St>
where
    St: Stream<Item = std::result::Result<Message, E>> + Unpin + Send,
    E: std::error::Error + Send + Sync + 'static,
    F: DeserializeOwned,
{
    async fn recv(&mut self) -> Result<Option<F>> {
        loop {
            let msg = match timeout_at(self.last_frame + self.deadline, self.stream.next()).await {
                Ok(Some(msg)) => msg?,
                Ok(None) => return Ok(None),
                Err(_) => anyhow::bail!("no frame received in {:?}", self.deadline),
            };
            self.last_frame = Instant::now();
            match msg {
                Message::Binary(sealed) => {
                    let plaintext = self.cipher.open(&sealed)?;
                    return Ok(Some(serde_json::from_slice(&plaintext)?));
                }
                Message::Close(_) => return Ok(None),
                // Control frames carry no payload but still reset the deadline above.
                _ => continue,
            }
        }
    }
}

impl<Si, F> FrameWriter<F> for WsWriter<Si>
where
    Si: Sink<Message> + Unpin + Send,
    Si::Error: std::error::Error + Send + Sync + 'static,
    F: Serialize + Sync,
{
    async fn send(&mut self, frame: &F) -> Result<()> {
        let bytes = serde_json::to_vec(frame)?;
        let sealed = self.cipher.seal(&bytes);
        self.sink.send(Message::Binary(sealed)).await?;
        Ok(())
    }

    async fn keepalive(&mut self) -> Result<()> {
        self.sink.send(Message::Ping(Vec::new())).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[test]
    fn event_serde_round_trip() {
        let ev = ControllerEvent::ToolResult {
            id: "t1".into(),
            content: "line one\nline two\twith tab".into(),
            is_error: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(
            !json.contains('\n'),
            "compact JSON leaked a raw newline: {json}"
        );
        let back: ControllerEvent = serde_json::from_str(&json).unwrap();
        match back {
            ControllerEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                assert_eq!(id, "t1");
                assert_eq!(content, "line one\nline two\twith tab");
                assert!(!is_error);
            }
            other => panic!("round-trip changed the variant: {other:?}"),
        }
    }

    type WsItem = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>;

    fn ws_reader<St>(stream: St, cipher: Cipher, deadline: Duration) -> WsReader<St> {
        WsReader {
            stream,
            cipher,
            deadline,
            last_frame: Instant::now(),
        }
    }

    #[tokio::test]
    async fn ws_read_deadline_fires_on_a_silent_socket() {
        let stream = futures::stream::pending::<WsItem>();
        let mut reader = ws_reader(stream, Cipher::generate(), Duration::from_millis(100));
        let res = tokio::time::timeout(Duration::from_secs(2), async {
            FrameReader::<ServerFrame>::recv(&mut reader).await
        })
        .await
        .expect("the read deadline never fired");
        assert!(res.is_err(), "expected a deadline error, got {res:?}");
    }

    // The deadline must survive `recv` calls cancelled by a caller's `select!`.
    #[tokio::test]
    async fn ws_control_frames_keep_the_read_deadline_at_bay() {
        let cipher = Cipher::generate();
        let sealed = cipher.seal(&serde_json::to_vec(&ServerFrame::Attached).unwrap());
        let (tx, rx) = futures::channel::mpsc::unbounded::<WsItem>();
        let mut reader = ws_reader(rx, cipher, Duration::from_millis(150));
        tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                tx.unbounded_send(Ok(Message::Pong(Vec::new()))).unwrap();
            }
            tx.unbounded_send(Ok(Message::Binary(sealed))).unwrap();
            std::future::pending::<()>().await;
        });

        let got = loop {
            tokio::select! {
                frame = FrameReader::<ServerFrame>::recv(&mut reader) => break frame,
                _ = tokio::time::sleep(Duration::from_millis(30)) => {}
            }
        };
        assert!(
            matches!(got, Ok(Some(ServerFrame::Attached))),
            "expected the frame after the pongs, got {got:?}"
        );
    }

    #[tokio::test]
    async fn framed_write_then_read_in_order() {
        let (client, server) = tokio::io::duplex(256);
        let mut reader = BufReader::new(server);

        let writer = tokio::spawn(async move {
            let mut w = client;
            write_frame(
                &mut w,
                &ServerFrame::Event {
                    seq: 0,
                    event: ControllerEvent::AssistantText { text: "hi".into() },
                },
            )
            .await
            .unwrap();
            write_frame(&mut w, &ServerFrame::Attached).await.unwrap();
        });

        match read_frame::<_, ServerFrame>(&mut reader).await.unwrap() {
            Some(ServerFrame::Event { seq, event }) => {
                assert_eq!(seq, 0);
                assert!(matches!(event, ControllerEvent::AssistantText { text } if text == "hi"));
            }
            other => panic!("expected first Event frame, got {other:?}"),
        }
        assert!(matches!(
            read_frame::<_, ServerFrame>(&mut reader).await.unwrap(),
            Some(ServerFrame::Attached)
        ));
        assert!(
            read_frame::<_, ServerFrame>(&mut reader)
                .await
                .unwrap()
                .is_none(),
            "expected clean EOF after the writer dropped"
        );

        writer.await.unwrap();
    }

    #[tokio::test]
    async fn client_frame_round_trip() {
        let (a, b) = tokio::io::duplex(256);
        let mut reader = BufReader::new(b);
        let mut writer = a;

        write_frame(
            &mut writer,
            &ClientFrame::Command(UiEvent::PermissionResponse {
                tool_use_id: "t1".into(),
                allow: true,
            }),
        )
        .await
        .unwrap();
        write_frame(
            &mut writer,
            &ClientFrame::Attach {
                after_seq: Some(7),
                who: ClientIdentity::human("alice"),
                reverse_offer: false,
            },
        )
        .await
        .unwrap();

        match read_frame::<_, ClientFrame>(&mut reader).await.unwrap() {
            Some(ClientFrame::Command(UiEvent::PermissionResponse { tool_use_id, allow })) => {
                assert_eq!(tool_use_id, "t1");
                assert!(allow);
            }
            other => panic!("expected Command(PermissionResponse), got {other:?}"),
        }
        match read_frame::<_, ClientFrame>(&mut reader).await.unwrap() {
            Some(ClientFrame::Attach { after_seq, who, .. }) => {
                assert_eq!(after_seq, Some(7));
                assert_eq!(who.name, "alice");
                assert!(matches!(who.kind, crate::core::identity::ClientKind::Human));
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }

    // A non-offering Attach must omit `reverse_offer` so phone clients see unchanged bytes.
    #[test]
    fn reverse_edge_frames_wire_bytes_and_round_trip() {
        let plain = ClientFrame::Attach {
            after_seq: None,
            who: ClientIdentity::human("alice"),
            reverse_offer: false,
        };
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"Attach":{"after_seq":null,"who":{"kind":"Human","name":"alice","session_id":null,"task":null}}}"#
        );
        let offering = ClientFrame::Attach {
            after_seq: None,
            who: ClientIdentity::human("alice"),
            reverse_offer: true,
        };
        assert_eq!(
            serde_json::to_string(&offering).unwrap(),
            r#"{"Attach":{"after_seq":null,"who":{"kind":"Human","name":"alice","session_id":null,"task":null},"reverse_offer":true}}"#
        );

        let rev_ev = ClientFrame::ReverseEvent {
            seq: 3,
            event: ControllerEvent::AssistantText { text: "hi".into() },
        };
        match serde_json::from_str::<ClientFrame>(&serde_json::to_string(&rev_ev).unwrap()).unwrap()
        {
            ClientFrame::ReverseEvent { seq, event } => {
                assert_eq!(seq, 3);
                assert!(matches!(event, ControllerEvent::AssistantText { text } if text == "hi"));
            }
            other => panic!("expected ReverseEvent, got {other:?}"),
        }

        let rev_att = ServerFrame::ReverseAttach {
            who: ClientIdentity {
                kind: crate::core::identity::ClientKind::Agent,
                name: "peer-b".into(),
                session_id: Some("s1".into()),
                task: None,
            },
            supervised: true,
        };
        match serde_json::from_str::<ServerFrame>(&serde_json::to_string(&rev_att).unwrap())
            .unwrap()
        {
            ServerFrame::ReverseAttach { who, supervised } => {
                assert_eq!(who.name, "peer-b");
                assert!(matches!(who.kind, crate::core::identity::ClientKind::Agent));
                assert!(supervised);
            }
            other => panic!("expected ReverseAttach, got {other:?}"),
        }
        let unsupervised = ServerFrame::ReverseAttach {
            who: ClientIdentity::human("h"),
            supervised: false,
        };
        assert!(
            !serde_json::to_string(&unsupervised)
                .unwrap()
                .contains("supervised"),
            "an unsupervised ReverseAttach must serialize as before"
        );

        let rev_cmd = ServerFrame::ReverseCommand(UiEvent::UserMessage { text: "go".into() });
        match serde_json::from_str::<ServerFrame>(&serde_json::to_string(&rev_cmd).unwrap())
            .unwrap()
        {
            ServerFrame::ReverseCommand(UiEvent::UserMessage { text }) => assert_eq!(text, "go"),
            other => panic!("expected ReverseCommand(UserMessage), got {other:?}"),
        }
    }

    // Pinned so the Kotlin mirror (FramesTest) can assert the same strings.
    #[test]
    fn command_and_capabilities_wire_bytes() {
        use crate::core::events::{CommandInfo, PeerInfo};
        use crate::core::{ClientKind, ControllerEvent, McpServerInfo, ModelInfo};

        let cmd = ClientFrame::Command(UiEvent::Command {
            line: "/model x".into(),
        });
        assert_eq!(
            serde_json::to_string(&cmd).unwrap(),
            r#"{"Command":{"Command":{"line":"/model x"}}}"#
        );

        let cap = ControllerEvent::Capabilities {
            commands: vec![CommandInfo {
                name: "/model".into(),
                usage: "u".into(),
            }],
            models: vec![ModelInfo {
                id: "id1".into(),
                label: "L1".into(),
            }],
            mcp: vec![McpServerInfo {
                name: "gitlab".into(),
                description: Some("d".into()),
                loaded: false,
            }],
            peers: vec![PeerInfo {
                name: "child-ab12cd34".into(),
                kind: ClientKind::Agent,
                supervised: true,
                session_id: Some("s1".into()),
                task: None,
            }],
        };
        assert_eq!(
            serde_json::to_string(&cap).unwrap(),
            r#"{"Capabilities":{"commands":[{"name":"/model","usage":"u"}],"models":[{"id":"id1","label":"L1"}],"mcp":[{"name":"gitlab","description":"d","loaded":false}],"peers":[{"name":"child-ab12cd34","kind":"Agent","supervised":true,"session_id":"s1","task":null}]}}"#
        );
    }

    // Pinned for the Kotlin mirror (FramesTest). A frame without `auto` must still
    // decode, as auto off.
    #[test]
    fn session_info_wire_bytes() {
        use crate::core::ControllerEvent;

        let info = ControllerEvent::SessionInfo {
            model: "m".into(),
            cwd: "/tmp".into(),
            git_branch: None,
            session_id: "id".into(),
            session_name: None,
            auto: true,
        };
        assert_eq!(
            serde_json::to_string(&info).unwrap(),
            r#"{"SessionInfo":{"model":"m","cwd":"/tmp","git_branch":null,"session_id":"id","session_name":null,"auto":true}}"#
        );

        let legacy: ControllerEvent = serde_json::from_str(
            r#"{"SessionInfo":{"model":"m","cwd":"/tmp","git_branch":null,"session_id":"id","session_name":null}}"#,
        )
        .unwrap();
        assert!(matches!(
            legacy,
            ControllerEvent::SessionInfo { auto: false, .. }
        ));
    }
}
