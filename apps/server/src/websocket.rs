use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::response::Response;
use tracing::info;

pub async fn agent_ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_agent_socket)
}

pub async fn client_ws_handler(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(handle_client_socket)
}

async fn handle_agent_socket(_socket: WebSocket) {
    info!("agent websocket connected");
    // TODO: Implement agent WebSocket protocol
    // - Read registration token from first message
    // - Validate token against database
    // - Mark machine as online
    // - Relay signaling messages between client and agent
}

async fn handle_client_socket(_socket: WebSocket) {
    info!("client websocket connected");
    // TODO: Implement client WebSocket protocol
    // - Validate JWT session
    // - Send machine list
    // - Handle connection requests
    // - Relay signaling messages
}
