use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use crate::signaling::ConnectionRequest;

/// WebSocket client for the SaaS coordination server.
///
/// Holds a **single persistent** connection: a reader task surfaces inbound
/// `ConnectRequest`s and answers heartbeats, while outbound signaling
/// (answers, ICE candidates) is sent over the same socket via an mpsc queue.
///
/// (A previous version opened a fresh WebSocket per message, which — with the
/// relay's per-connection registration — would repeatedly flap the machine
/// offline and evict the persistent registration.)
pub struct ServerClient {
    outbound: UnboundedSender<Message>,
    /// Reader + writer tasks, aborted when the client is dropped so a discarded
    /// connection (e.g. during a reconnect) doesn't leak tasks or the socket.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for ServerClient {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl ServerClient {
    /// Connect, send `hello`, and start the reader/writer tasks. Returns the
    /// client plus a receiver of inbound connection requests.
    pub async fn connect(
        url: &str,
        token: &str,
    ) -> Result<(Self, UnboundedReceiver<ConnectionRequest>)> {
        let url_with_token = format!("{}?token={}", url, urlencoding::encode(token));
        let (ws_stream, _) = connect_async(&url_with_token)
            .await
            .context("Failed to connect to SaaS server WebSocket")?;

        let (mut write, mut read) = ws_stream.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (req_tx, req_rx) = mpsc::unbounded_channel::<ConnectionRequest>();

        // Announce ourselves.
        let hello = serde_json::json!({ "type": "hello", "version": env!("CARGO_PKG_VERSION") });
        out_tx
            .send(Message::Text(hello.to_string()))
            .ok()
            .context("failed to enqueue hello")?;

        // Writer task: drain the outbound queue onto the socket.
        let writer = tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if let Err(e) = write.send(msg).await {
                    tracing::warn!(error = %e, "failed to send on agent websocket");
                    break;
                }
            }
        });

        // Reader task: dispatch inbound messages.
        let reader_out = out_tx.clone();
        let reader = tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(signal) = serde_json::from_str::<ServerSignal>(&text) {
                            match signal {
                                ServerSignal::ConnectRequest { session_id, offer }
                                | ServerSignal::SignalingOffer { session_id, offer } => {
                                    let _ = req_tx.send(ConnectionRequest { session_id, offer });
                                }
                                ServerSignal::Heartbeat => {
                                    let hb = serde_json::json!({ "type": "heartbeat" });
                                    let _ = reader_out.send(Message::Text(hb.to_string()));
                                }
                                _ => tracing::debug!("received signal: {:?}", signal),
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        tracing::warn!("server closed agent websocket connection");
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok((
            Self {
                outbound: out_tx,
                tasks: vec![writer, reader],
            },
            req_rx,
        ))
    }

    /// Send an SDP answer back to the client via the server.
    pub fn send_answer(&self, session_id: &str, answer: &str) -> Result<()> {
        let msg = serde_json::json!({
            "type": "signaling_answer",
            "session_id": session_id,
            "answer": answer,
        });
        self.outbound
            .send(Message::Text(msg.to_string()))
            .context("agent websocket closed")
    }

    /// Send an ICE candidate to the client via the server.
    ///
    /// Reserved for trickle ICE; the current flow uses non-trickle (candidates
    /// embedded in the answer SDP), so this is not yet called.
    #[allow(dead_code)]
    pub fn send_ice_candidate(&self, session_id: &str, candidate: &str) -> Result<()> {
        let msg = serde_json::json!({
            "type": "ice_candidate",
            "session_id": session_id,
            "candidate": candidate,
        });
        self.outbound
            .send(Message::Text(msg.to_string()))
            .context("agent websocket closed")
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerSignal {
    #[serde(rename = "connect_request")]
    ConnectRequest { session_id: String, offer: String },
    #[serde(rename = "signaling_offer")]
    SignalingOffer { session_id: String, offer: String },
    #[serde(rename = "ice_candidate")]
    IceCandidate {
        #[allow(dead_code)]
        session_id: String,
        #[allow(dead_code)]
        candidate: String,
    },
    #[serde(rename = "heartbeat")]
    Heartbeat,
    #[serde(rename = "machine_status")]
    MachineStatus,
}
