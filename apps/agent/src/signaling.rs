/// Connection request from the SaaS server.
#[derive(Debug, Clone)]
pub struct ConnectionRequest {
    pub session_id: String,
    pub offer: String,
}
