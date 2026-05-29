//! Client-side WebRTC session establishment over the server signaling relay.
//!
//! Mirrors the agent's answerer flow: the client is the **offerer**. It opens a
//! `/client` signaling WebSocket, sends a self-contained (non-trickle) offer in
//! a `ConnectRequest`, applies the relayed answer, and waits for the `control`
//! DataChannel to open. The returned [`PeerSession`] is then ready to carry
//! input events to the host.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use remotekvm_transport::PeerSession;
use serde_json::{json, Value};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(20);
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// Establish a connected [`PeerSession`] for `session_id`.
///
/// `ws_base` is the server's WebSocket base (e.g. `ws://localhost:8080`);
/// `jwt` is the session token; `session_id` comes from `POST /machines/:id/connect`.
pub async fn establish_session(ws_base: &str, jwt: &str, session_id: &str) -> Result<PeerSession> {
    let url = format!("{}/client?token={}", ws_base.trim_end_matches('/'), jwt);
    let (mut ws, _) = connect_async(&url)
        .await
        .context("failed to open client signaling websocket")?;

    let session = PeerSession::offerer().await?;
    session.create_offer().await?;
    let offer = session.wait_ice_gathering().await?;

    let req = json!({
        "type": "connect_request",
        "session_id": session_id,
        "client_id": "",
        "offer": offer,
    });
    ws.send(Message::Text(req.to_string()))
        .await
        .context("failed to send connect_request")?;

    // Wait for the relayed answer (ConnectResponse).
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, wait_for_answer(&mut ws))
        .await
        .map_err(|_| anyhow!("timed out waiting for the host's answer"))??;
    session.set_answer(&answer).await?;

    session.wait_channel_open(CHANNEL_OPEN_TIMEOUT).await?;
    Ok(session)
}

async fn wait_for_answer(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Result<String> {
    while let Some(msg) = ws.next().await {
        if let Message::Text(text) = msg.context("signaling websocket error")? {
            let v: Value = serde_json::from_str(&text).context("invalid signaling JSON")?;
            if v["type"] == "connect_response" {
                if let Some(answer) = v["answer"].as_str() {
                    return Ok(answer.to_string());
                }
                return Err(anyhow!("connect_response missing answer"));
            }
        }
    }
    Err(anyhow!(
        "signaling websocket closed before an answer arrived"
    ))
}
