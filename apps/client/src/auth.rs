use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const KEYRING_SERVICE: &str = "io.anchras.remotekvm";
const KEYRING_USER: &str = "session-jwt";

/// WorkOS AuthKit authentication flow for a native desktop app.
///
/// The native app cannot receive a server-side redirect directly, so we use the
/// "loopback redirect" pattern (the same one the gcloud / GitHub CLIs use):
///
/// 1. Bind a [`LoopbackReceiver`] on `127.0.0.1:<ephemeral>`.
/// 2. Open the system browser to the server's login endpoint, passing the
///    loopback `redirect_uri`. The server runs the WorkOS AuthKit exchange and,
///    once it has minted our JWT, redirects the browser back to
///    `http://127.0.0.1:<port>/callback?token=<jwt>`.
/// 3. The receiver captures that one request and extracts the token.
pub struct AuthFlow {
    server_url: String,
}

impl AuthFlow {
    pub fn new(server_url: &str) -> Self {
        Self {
            server_url: server_url.trim_end_matches('/').to_string(),
        }
    }

    /// Build the URL to open in the browser to begin login.
    ///
    /// `redirect_uri` is the loopback URL the server should send the browser
    /// back to once authentication succeeds.
    pub fn login_url(&self, redirect_uri: &str) -> String {
        let encoded = urlencode(redirect_uri);
        format!("{}/auth/login?redirect_uri={}", self.server_url, encoded)
    }

    /// Open the system browser at the login URL.
    pub fn open_browser(&self, redirect_uri: &str) -> Result<()> {
        let url = self.login_url(redirect_uri);
        tracing::info!(%url, "opening browser for WorkOS AuthKit login");
        open::that(&url).context("failed to open system browser")?;
        Ok(())
    }
}

/// A one-shot HTTP listener on loopback that captures the OAuth callback.
pub struct LoopbackReceiver {
    listener: std::net::TcpListener,
    port: u16,
}

impl LoopbackReceiver {
    /// Bind to an ephemeral loopback port.
    ///
    /// Synchronous on purpose: callers (e.g. the egui event loop) may run inside
    /// an active Tokio context where `Handle::block_on` would panic. Binding a
    /// `std` listener needs no runtime; it's converted to a Tokio listener in
    /// [`Self::wait_for_token`], which runs on a spawned task.
    pub fn bind() -> Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("failed to bind loopback listener")?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// The redirect URI the browser will be sent back to.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Wait for the single callback request and return the `token` query value.
    ///
    /// Returns an error if the callback carries an `error` parameter, omits the
    /// token, or the timeout elapses first.
    pub async fn wait_for_token(self, timeout: Duration) -> Result<String> {
        self.listener
            .set_nonblocking(true)
            .context("failed to set loopback listener non-blocking")?;
        let listener = TcpListener::from_std(self.listener)
            .context("failed to adopt loopback listener into the runtime")?;
        let (stream, _) = tokio::time::timeout(timeout, listener.accept())
            .await
            .map_err(|_| anyhow!("timed out waiting for the login callback"))?
            .context("failed to accept loopback connection")?;
        handle_callback(stream).await
    }
}

async fn handle_callback(mut stream: TcpStream) -> Result<String> {
    // Read until we have the full request line. A single `read` may return only
    // a partial line, so accumulate until we see a line terminator (or fill the
    // buffer / hit EOF).
    let mut buf = vec![0u8; 8192];
    let mut filled = 0;
    loop {
        let n = stream
            .read(&mut buf[filled..])
            .await
            .context("failed to read callback request")?;
        if n == 0 {
            break; // EOF
        }
        filled += n;
        if buf[..filled].contains(&b'\n') || filled == buf.len() {
            break;
        }
    }
    let request = String::from_utf8_lossy(&buf[..filled]);
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow!("empty callback request"))?;

    // `GET /callback?token=... HTTP/1.1`
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed request line: {request_line:?}"))?;

    let result = extract_token(path);

    // Always answer the browser so the user gets a clean "you can close this" page.
    let (status, body) = match &result {
        Ok(_) => (
            "200 OK",
            "<html><body><h2>RemoteKVM</h2><p>Login complete. You can close this window.</p></body></html>",
        ),
        Err(_) => (
            "400 Bad Request",
            "<html><body><h2>RemoteKVM</h2><p>Login failed. Please return to the app and try again.</p></body></html>",
        ),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;

    result
}

/// Extract the `token` query parameter from a request path, surfacing any
/// `error` parameter as an error.
fn extract_token(path: &str) -> Result<String> {
    let url = url::Url::parse(&format!("http://localhost{path}"))
        .context("failed to parse callback path")?;

    let mut token = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "error" => return Err(anyhow!("login failed: {v}")),
            "token" => token = Some(v.into_owned()),
            _ => {}
        }
    }
    token.ok_or_else(|| anyhow!("callback did not include a token"))
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Persist the session JWT in the OS secure store (macOS Keychain, Windows
/// Credential Manager, or libsecret on Linux) via the `keyring` crate.
pub struct TokenStore;

impl TokenStore {
    /// A single process-wide credential handle. The `keyring` `Entry` is the
    /// stable identity for our secret; caching one avoids re-resolving it on
    /// every call and keeps state consistent under the in-memory mock backend
    /// used by tests.
    fn entry() -> Result<&'static keyring::Entry> {
        use std::sync::OnceLock;
        static ENTRY: OnceLock<keyring::Entry> = OnceLock::new();
        if let Some(e) = ENTRY.get() {
            return Ok(e);
        }
        let e = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .context("failed to open secure credential store")?;
        let _ = ENTRY.set(e); // ignore race: another thread won, value identical
        Ok(ENTRY.get().expect("entry just set"))
    }

    /// Save the JWT to secure storage.
    pub fn save_token(token: &str) -> Result<()> {
        Self::entry()?
            .set_password(token)
            .context("failed to store session token")
    }

    /// Load the JWT from secure storage, returning `None` if none is stored.
    pub fn load_token() -> Result<Option<String>> {
        match Self::entry()?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(anyhow!("failed to read session token: {e}")),
        }
    }

    /// Delete the stored token (logout). A missing token is not an error.
    pub fn delete_token() -> Result<()> {
        match Self::entry()?.delete_password() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(anyhow!("failed to delete session token: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_url_encodes_redirect() {
        let flow = AuthFlow::new("https://api.example.com/");
        let url = flow.login_url("http://127.0.0.1:54321/callback");
        assert_eq!(
            url,
            "https://api.example.com/auth/login?redirect_uri=http%3A%2F%2F127.0.0.1%3A54321%2Fcallback"
        );
    }

    #[test]
    fn extract_token_parses_value() {
        assert_eq!(
            extract_token("/callback?token=abc.def.ghi").unwrap(),
            "abc.def.ghi"
        );
    }

    #[test]
    fn extract_token_surfaces_error_param() {
        let err = extract_token("/callback?error=access_denied").unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[test]
    fn extract_token_missing_is_error() {
        assert!(extract_token("/callback").is_err());
    }

    #[tokio::test]
    async fn loopback_receiver_captures_token_from_redirect() {
        let receiver = LoopbackReceiver::bind().unwrap();
        let redirect = receiver.redirect_uri();
        assert!(redirect.starts_with("http://127.0.0.1:"));

        // Simulate the browser following the server's redirect.
        let redirect_with_token = format!("{redirect}?token=header.payload.sig");
        let fetch =
            tokio::spawn(async move { reqwest::get(&redirect_with_token).await.unwrap().status() });

        let token = receiver
            .wait_for_token(Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(token, "header.payload.sig");
        assert_eq!(fetch.await.unwrap(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn loopback_receiver_reports_oauth_error() {
        let receiver = LoopbackReceiver::bind().unwrap();
        let redirect = receiver.redirect_uri();
        let url = format!("{redirect}?error=access_denied");
        tokio::spawn(async move {
            let _ = reqwest::get(&url).await;
        });
        let err = receiver
            .wait_for_token(Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("access_denied"));
    }

    #[test]
    fn token_store_round_trips_via_mock_keyring() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        // Route keyring at a process-local mock so the test never touches the
        // real OS Keychain (which would prompt / require entitlements).
        INIT.call_once(|| {
            keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        });

        TokenStore::save_token("jwt-123").unwrap();
        assert_eq!(
            TokenStore::load_token().unwrap().as_deref(),
            Some("jwt-123")
        );
        TokenStore::delete_token().unwrap();
        // Mock store returns NoEntry after deletion -> None.
        assert_eq!(TokenStore::load_token().unwrap(), None);
        // Deleting again is a no-op, not an error.
        TokenStore::delete_token().unwrap();
    }
}
