use crate::auth::workos::WorkOsClient;
use crate::config::Config;
use crate::db::Database;
use crate::websocket::SignalingState;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub workos: WorkOsClient,
    pub config: Config,
    pub signaling: SignalingState,
}
