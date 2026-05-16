use anyhow::Result;

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub workos_api_key: String,
    pub workos_client_id: String,
    pub jwt_secret: String,
    pub jwt_expiry_hours: i64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port = std::env::var("PORT")
            .unwrap_or_else(|_| "8080".to_string())
            .parse()?;

        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/remotekvm".to_string());

        let workos_api_key = std::env::var("WORKOS_API_KEY")
            .unwrap_or_else(|_| "".to_string());

        let workos_client_id = std::env::var("WORKOS_CLIENT_ID")
            .unwrap_or_else(|_| "".to_string());

        let jwt_secret = std::env::var("JWT_SECRET")
            .unwrap_or_else(|_| "change-me-in-production".to_string());

        let jwt_expiry_hours = std::env::var("JWT_EXPIRY_HOURS")
            .unwrap_or_else(|_| "24".to_string())
            .parse()?;

        Ok(Config {
            port,
            database_url,
            workos_api_key,
            workos_client_id,
            jwt_secret,
            jwt_expiry_hours,
        })
    }
}
