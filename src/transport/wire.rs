use std::future::Future;

use anyhow::Result;
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use super::encryption::Cipher;
use crate::core::{ClientIdentity, ControllerEvent, UiEvent};

// The serializable form of the in-memory `Controller`/`UiEvent` model from 8.0.
// 8.1 only *transports* that model — the broker's buffer/replay and permission
// correlation already exist in memory; here we frame it onto a byte stream
// (the TUI↔daemon Unix socket now, the relayed WebSocket later).
//
// Encoding is newline-delimited JSON: `serde_json` compact output contains no
// literal newline (any newline inside string data is escaped as `\n`), so a
// single `\n` is an unambiguous frame terminator and the stream stays readable
// for debugging. Switch to a length-prefixed binary codec only if relay
// throughput on long tool outputs ever justifies it (a deferred open question).

// Client → daemon.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ClientFrame {
    // Bind to the session. `after_seq` is the resume cursor: replay only events
    // with `seq > after_seq`, so a client whose connection dropped catches up on
    // exactly what it missed. `None` = fresh attach → full replay from seq 0. `who`
    // is the attaching client's identity — the attach frame is the handshake.
    Attach {
        after_seq: Option<u64>,
        who: ClientIdentity,
    },
    // Yield the session without ending it — the loop keeps running headless and
    // buffering. (Distinct from dropping the connection, though the daemon
    // treats a dropped socket as an implicit detach too.)
    Detach,
    // An application-level command (user message, model switch, MCP, permission
    // answer, quit). Maps straight to the in-memory `UiEvent`.
    Command(UiEvent),
}

// Daemon → client.
#[derive(Debug, Serialize, serde::Deserialize)]
pub enum ServerFrame {
    // Attach accepted; the controller event stream (replay-from-cursor, then
    // live) follows as `Event` frames.
    Attached,
    // Attach could not be served: the session is gone (the daemon's broker has shut
    // down). The client should not expect any `Event` frames.
    Busy,
    // One controller event tagged with its monotonic sequence number. `seq`
    // counts every event the session has ever emitted (replay + live share one
    // sequence), so it is a stable resume cursor across reconnects.
    Event { seq: u64, event: ControllerEvent },
}

// Write one frame as a single newline-terminated JSON line and flush. Flushing
// per frame keeps latency low for an interactive control channel — frames are
// small and infrequent relative to throughput limits.
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

// Read one newline-delimited frame. `Ok(None)` is a clean EOF (the peer closed
// the connection) — the caller distinguishes that from an error to tell an
// orderly disconnect apart from a transport fault. The reader must be buffered
// (`BufReader`) so `read_line` doesn't issue a syscall per byte.
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncBufReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    let n = r.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None); // clean EOF
    }
    let frame = serde_json::from_str(line.trim_end())?;
    Ok(Some(frame))
}

// ── Frame-level transport seam ───────────────────────────────────────────────
// The codec above delimits frames with newlines because a Unix socket is a raw
// *byte* stream with no message boundaries. A WebSocket is *message*-shaped and
// supplies its own boundaries, so its codec (added with the relay path) maps one
// frame to exactly one WS message and never newline-frames — re-framing on top of
// WS messages would be redundant. These two traits are that seam, one frame at a
// time: the daemon's `handle_conn` and the socket client's bridge tasks are
// generic over them, so the same session logic drives either transport and only
// the codec differs. Read and write are separate halves because both sides own the
// two directions independently — the client in two tasks, the daemon in a `select!`.
pub trait FrameReader<F>: Send {
    // `Ok(None)` = clean EOF (peer closed); `Err` = transport or parse fault.
    fn recv(&mut self) -> impl Future<Output = Result<Option<F>>> + Send;
}

pub trait FrameWriter<F>: Send {
    fn send(&mut self, frame: &F) -> impl Future<Output = Result<()>> + Send;
}

// The newline-delimited-JSON codec over a raw byte stream — the Unix-socket
// transport. A named pair (rather than a blanket impl over every `AsyncRead`/
// `AsyncWrite`) so it reads as "the line codec", parallel to the WS codec to come.
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

// The WebSocket codec — the relayed transport. A WebSocket is message-framed, so
// one protocol frame maps to exactly one WS Binary message and there's no newline
// framing (re-framing on top of WS messages would be redundant). Each frame is
// sealed with the app-layer [`Cipher`] before it becomes a Message and opened on
// receipt, so the relay only ever forwards opaque ciphertext (8.2-d). Generic over
// the underlying sink/stream so it works for any tokio-tungstenite connection.
pub struct WsReader<St> {
    stream: St,
    cipher: Cipher,
}
pub struct WsWriter<Si> {
    sink: Si,
    cipher: Cipher,
}

impl<St> WsReader<St> {
    pub fn new(stream: St, cipher: Cipher) -> Self {
        Self { stream, cipher }
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
        while let Some(msg) = self.stream.next().await {
            match msg? {
                Message::Binary(sealed) => {
                    let plaintext = self.cipher.open(&sealed)?;
                    return Ok(Some(serde_json::from_slice(&plaintext)?));
                }
                // A Close (like a byte-stream EOF) ends the frame stream cleanly.
                Message::Close(_) => return Ok(None),
                // Only Binary carries sealed application frames; ping/pong/text/
                // continuation frames have no payload for us — skip and keep reading.
                _ => continue,
            }
        }
        Ok(None) // stream ended without a Close frame
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
        // `SinkExt::send` feeds + flushes, matching the line codec's flush-per-frame
        // so an interactive control channel stays low-latency.
        self.sink.send(Message::Binary(sealed)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    // A representative event round-trips through JSON unchanged.
    #[test]
    fn event_serde_round_trip() {
        let ev = ControllerEvent::ToolResult {
            id: "t1".into(),
            content: "line one\nline two\twith tab".into(),
            is_error: false,
        };
        let json = serde_json::to_string(&ev).unwrap();
        // Compact JSON must not contain a raw newline — the framing relies on it.
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

    // Frames written back-to-back are read back in order, and EOF reads as None.
    #[tokio::test]
    async fn framed_write_then_read_in_order() {
        let (client, server) = tokio::io::duplex(256);
        let mut reader = BufReader::new(server);

        // Writer task: emit two frames then drop the stream → EOF on the reader.
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

        // Reader sees the two frames in order, then a clean EOF.
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

    // A ClientFrame carrying a UiEvent round-trips across the same framing.
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
            Some(ClientFrame::Attach { after_seq, who }) => {
                assert_eq!(after_seq, Some(7));
                assert_eq!(who.name, "alice");
                assert!(matches!(who.kind, crate::core::identity::ClientKind::Human));
            }
            other => panic!("expected Attach, got {other:?}"),
        }
    }
}
