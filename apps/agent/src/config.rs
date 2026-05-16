use anyhow::Result;

pub struct Config {
    pub server_url: String,
    pub registration_token: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let server_url = std::env::var("RKVM_SERVER_URL")
            .unwrap_or_else(|_| "ws://localhost:8080/agent".to_string());

        let registration_token = std::env::var("RKVM_REGISTRATION_TOKEN")
            .map_err(|_| anyhow::anyhow!("RKVM_REGISTRATION_TOKEN must be set"))?;

        let width = std::env::var("RKVM_WIDTH")
            .unwrap_or_else(|_| "1920".to_string())
            .parse()?;

        let height = std::env::var("RKVM_HEIGHT")
            .unwrap_or_else(|_| "1080".to_string())
            .parse()?;

        let fps = std::env::var("RKVM_FPS")
            .unwrap_or_else(|_| "60".to_string())
            .parse()?;

        let bitrate_kbps = std::env::var("RKVM_BITRATE_KBPS")
            .unwrap_or_else(|_| "20000".to_string())
            .parse()?;

        Ok(Config {
            server_url,
            registration_token,
            width,
            height,
            fps,
            bitrate_kbps,
        })
    }
}
