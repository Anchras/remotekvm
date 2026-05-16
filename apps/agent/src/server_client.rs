use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures::{SinkExt, StreamExt};

use crate::signaling::ConnectionRequest;

/// WebSocket client for connecting to the SaaS coordination server.
pub struct ServerClient {
    url: String,
    token: String,
}

impl ServerClient {
    /// Connect to the server WebSocket endpoint.
    pub async fn connect(url: &str, token: &str) -> Result<Self> {
        // The server expects the token as a query parameter: /agent?token=<token>
        let url_with_token = format!("{}?token={}", url, urlencoding::encode(token));

        // Test the connection
        let (ws_stream, _) = connect_async(&url_with_token)
            .await
            .context("Failed to connect to SaaS server WebSocket")?;

        // We don't keep the stream open here; reconnect on each use for simplicity
        // In production, use a persistent connection with automatic reconnection
        drop(ws_stream);

        Ok(ServerClient {
            url: url.to_string(),
            token: token.to_string(),
        })
    }

    /// Listen for messages from the server and forward connection requests.
    pub async fn handle_messages(&self, tx: UnboundedSender<ConnectionRequest>) -> Result<()> {
        let url_with_token = format!("{}?token={}", self.url, urlencoding::encode(&self.token));
        let (mut ws_stream, _) = connect_async(&url_with_token).await?;

        // Send hello
        let hello = serde_json::json!({
            "type": "hello",
            "version": env!("CARGO_PKG_VERSION")
        });
        ws_stream.send(Message::Text(hello.to_string())).await?;

        loop {
            tokio::select! {
                msg = ws_stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(signal) = serde_json::from_str::<ServerSignal>(&text) {
                                match signal {
                                    ServerSignal::ConnectRequest { session_id, offer } => {
                                        let _ = tx.send(ConnectionRequest {
                                            session_id,
                                            offer,
                                        });
                                    }
                                    ServerSignal::Heartbeat => {
                                        let heartbeat = serde_json::json!({"type": "heartbeat"});
                                        if let Err(e) = ws_stream.send(Message::Text(heartbeat.to_string())).await {
                                            tracing::warn!(error = %e, "failed to send heartbeat");
                                        }
                                    }
                                    _ => {
                                        tracing::debug!("received signal: {:?}", signal);
                                    }
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => {
                            tracing::warn!("server closed WebSocket connection");
                            break;
                        }
                        Some(Err(e)) => {
                            return Err(e.into());
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    /// Send an answer back to the client via the server.
    pub async fn send_answer(&self, session_id: &str, answer: &str) -> Result<()> {
        let url_with_token = format!("{}?token={}", self.url, urlencoding::encode(&self.token));
        let (mut ws_stream, _) = connect_async(&url_with_token).await?;

        let msg = serde_json::json!({
            "type": "signaling_answer",
            "session_id": session_id,
            "answer": answer
        });

        ws_stream.send(Message::Text(msg.to_string())).await?;
        Ok(())
    }

    /// Send an ICE candidate to the client via the server.
    pub async fn send_ice_candidate(&self, session_id: &str, candidate: &str) -> Result<()> {
        let url_with_token = format!("{}?token={}", self.url, urlencoding::encode(&self.token));
        let (mut ws_stream, _) = connect_async(&url_with_token).await?;

        let msg = serde_json::json!({
            "type": "ice_candidate",
            "session_id": session_id,
            "candidate": candidate
        });

        ws_stream.send(Message::Text(msg.to_string())).await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ServerSignal {
    #[serde(rename = "connect_request")]
    ConnectRequest { session_id: String, offer: String },
    #[serde(rename = "heartbeat")]
    Heartbeat,
    #[serde(rename = "machine_status")]
    MachineStatus,
}
