//! Client-side WebRTC session establishment over the server signaling relay.
//!
//! Mirrors the agent's answerer flow: the client is the **offerer**. It opens a
//! `/client` signaling WebSocket, sends a self-contained (non-trickle) offer in
//! a `ConnectRequest`, applies the relayed answer, and waits for the `control`
//! DataChannel to open. The returned [`ClientSession`] is then ready to carry
//! input events to the host.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, Stream, StreamExt};
use remotekvm_protocol::ChannelMessage;
use remotekvm_transport::PeerSession;
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use crate::media::{DecodedVideoFrame, MediaPipeline};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// A connected client-side WebRTC session plus its signaling WebSocket task.
pub struct ClientSession {
    peer: Arc<PeerSession>,
    media: MediaPipeline,
    signaling_task: JoinHandle<()>,
}

impl ClientSession {
    pub async fn send(&self, msg: &ChannelMessage) -> Result<()> {
        self.peer.send(msg).await
    }

    pub async fn close(&self) -> Result<()> {
        self.signaling_task.abort();
        self.peer.close().await
    }

    pub fn latest_video_frame(&self) -> Option<DecodedVideoFrame> {
        self.media.latest_video_frame()
    }
}

impl Drop for ClientSession {
    fn drop(&mut self) {
        self.signaling_task.abort();
    }
}

/// Establish a connected [`ClientSession`] for `session_id`.
///
/// `ws_base` is the server's WebSocket base (e.g. `ws://localhost:8080`);
/// `jwt` is the session token; `session_id` comes from `POST /machines/:id/connect`.
pub async fn establish_session(
    ws_base: &str,
    jwt: &str,
    session_id: &str,
) -> Result<ClientSession> {
    let token = url::form_urlencoded::byte_serialize(jwt.as_bytes()).collect::<String>();
    let url = format!("{}/client?token={}", ws_base.trim_end_matches('/'), token);
    let (ws, _) = connect_async(&url)
        .await
        .context("failed to open client signaling websocket")?;

    let session = Arc::new(PeerSession::offerer().await?);
    let media = MediaPipeline::new();
    media
        .attach_to_session(&session)
        .await
        .context("failed to attach client media receive pipeline")?;

    let (mut write, mut read) = ws.split();

    let hello = json!({
        "type": "hello",
        "version": env!("CARGO_PKG_VERSION"),
    });
    write
        .send(Message::Text(hello.to_string()))
        .await
        .context("failed to send client signaling hello")?;

    session.create_offer().await?;
    let offer = session.wait_ice_gathering().await?;

    let req = json!({
        "type": "connect_request",
        "session_id": session_id,
        "client_id": "",
        "offer": offer,
    });
    write
        .send(Message::Text(req.to_string()))
        .await
        .context("failed to send connect_request")?;

    // Wait for the relayed answer (ConnectResponse).
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, wait_for_answer(&mut read, &session))
        .await
        .map_err(|_| anyhow!("timed out waiting for the host's answer"))??;
    session.set_answer(&answer).await?;

    let signaling_session = session.clone();
    let signaling_session_id = session_id.to_string();
    let signaling_task = tokio::spawn(async move {
        let mut local_ice_done = false;
        loop {
            tokio::select! {
                msg = read.next() => {
                    let Some(msg) = msg else { break };
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Err(e) = handle_signal(&text, &signaling_session).await {
                                tracing::warn!(error = %e, "failed to handle signaling message");
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "client signaling websocket read failed");
                            break;
                        }
                    }
                }
                candidate = signaling_session.next_local_ice(), if !local_ice_done => {
                    let Some(candidate) = candidate else {
                        local_ice_done = true;
                        continue;
                    };
                    let msg = json!({
                        "type": "ice_candidate",
                        "session_id": signaling_session_id,
                        "candidate": candidate,
                    });
                    if let Err(e) = write.send(Message::Text(msg.to_string())).await {
                        tracing::warn!(error = %e, "failed to send local ICE candidate");
                        break;
                    }
                }
            }
        }
    });

    session.wait_channel_open(CHANNEL_OPEN_TIMEOUT).await?;
    Ok(ClientSession {
        peer: session,
        media,
        signaling_task,
    })
}

async fn wait_for_answer<S>(read: &mut S, session: &PeerSession) -> Result<String>
where
    S: Stream<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = read.next().await {
        if let Message::Text(text) = msg.context("signaling websocket error")? {
            let v = handle_signal(&text, session).await?;
            if v["type"] == "connect_response" {
                match v["status"].as_str() {
                    Some("accepted") | None => {}
                    Some(status) => return Err(anyhow!("host rejected session: {status}")),
                }
                return v["answer"]
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| anyhow!("connect_response missing answer"));
            }
        }
    }
    Err(anyhow!(
        "signaling websocket closed before an answer arrived"
    ))
}

async fn handle_signal(text: &str, session: &PeerSession) -> Result<Value> {
    let v: Value = serde_json::from_str(text).context("invalid signaling JSON")?;
    if v["type"] == "ice_candidate" {
        if let Some(candidate) = v["candidate"].as_str() {
            session
                .add_remote_ice(candidate.to_string())
                .await
                .context("failed to apply remote ICE candidate")?;
        }
    }
    Ok(v)
}
