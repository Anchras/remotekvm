use anyhow::Result;

pub struct Config {
    pub server_url: String,
    pub api_url: String,
    /// Base WebSocket URL for the signaling client (e.g. `ws://localhost:8080`).
    pub ws_url: String,
    /// Use `remotekvm://auth` instead of a loopback callback for browser auth.
    pub use_deep_link_auth: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let server_url = std::env::var("RKVM_SERVER_URL")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        let use_deep_link_auth = matches!(
            std::env::var("RKVM_AUTH_REDIRECT")
                .unwrap_or_else(|_| "loopback".to_string())
                .to_ascii_lowercase()
                .as_str(),
            "deeplink" | "deep-link" | "protocol" | "remotekvm"
        );

        let api_url = format!("{}/api", server_url);
        let ws_url = server_url
            .replace("http://", "ws://")
            .replace("https://", "wss://");

        Ok(Config {
            server_url,
            api_url,
            ws_url,
            use_deep_link_auth,
        })
    }
}
