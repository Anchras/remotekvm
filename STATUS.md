# RemoteKVM — Implementation Status

_Last updated: 2026-05-29_

This document is an honest, evidence-based account of what is implemented and
**locally verified**, versus what remains — and, for the remainder, *why* it
cannot be verified in a headless macOS development environment.

## TL;DR

- **Phase 0 (coordination server)** is complete and **verified with 32 automated
  tests** — incl. the WebSocket signaling relay, the native loopback OAuth login
  flow, and **Stripe billing (checkout + signed webhook)** against mock upstreams,
  all over real HTTP/WebSocket against a real Postgres.
- **The actual WebRTC data path works end-to-end**: a real peer-to-peer `control`
  DataChannel is negotiated between client and agent **through the live server
  relay**, and an `InputEvent` is delivered — verified in software over loopback
  (no GPU). The reusable `transport::PeerSession` powers both the agent and client.
- **Agent** connects over a single persistent WebSocket, answers connection
  requests, and **injects input on macOS via CoreGraphics** (mouse + keys + scroll).
- **Client** establishes the WebRTC session from the dashboard and **forwards
  egui mouse/keyboard input** over the control channel.
- **Still hardware/account-gated here**: Windows-native capture/encode/audio/
  input, the screen-capture→video-track pipeline (needs a display/GPU), the live
  WorkOS hosted UI, and live Stripe. See "Not verifiable in this environment".

## How to run what's verified

```sh
# 1. Postgres (a local override maps the container to 5433 to avoid clashing
#    with a native Postgres on 5432; see apps/server/docker-compose.override.yml)
cd apps/server && docker compose up -d postgres

# 2. Whole workspace builds and lints clean
cargo build --workspace
cargo clippy -p remotekvm-protocol -p remotekvm-transport \
             -p remotekvm-server -p remotekvm-client --all-targets -- -D warnings

# 3. Tests (server integration tests provision isolated DBs via #[sqlx::test])
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5433/remotekvm cargo test --workspace
```

## Verified (locally, with tests)

### Server (`apps/server`) — Phase 0 + Phase 4 billing
Refactored into a library + thin binary so the app can be built in-process for
tests. 14 integration tests in `tests/integration.rs`, each on an isolated
database, exercise:

- `GET /health`.
- JWT auth middleware: missing / malformed / wrong-secret tokens → 401.
- `GET /auth/workos/callback` against a **mock WorkOS server**: code exchange →
  user/org upsert → JWT issuance; re-login is idempotent (upsert, not duplicate)
  and still returns a usable token.
- Native loopback login: `GET /auth/login` 307-redirects to the WorkOS authorize
  URL carrying a **signed, short-lived `state`** (HMAC via JWT); the callback
  honors only a `state` it minted and redirects the token to a **loopback-only**
  `redirect_uri` (open-redirect and forged-state attempts → 400; `localhost`
  rejected as DNS-dependent).
- Machines CRUD: register / list / get / rotate-token / delete, with per-user
  permission isolation (another user gets 404, cannot delete).
- `POST /api/machines/:id/connect`: rejects offline machines and machines whose
  agent is not actually holding a live signaling socket (not just the stale DB
  `online` flag).
- **End-to-end WebSocket signaling relay**: agent registers → client offer
  relayed to agent → agent answer relayed to client → ICE relayed; bad agent
  token rejected at upgrade; a reconnecting agent does not get evicted by the old
  socket's teardown (connection-id guard).
- **Stripe billing** (Phase 4): `POST /api/billing/checkout` creates a session
  against a **mock Stripe** and returns the hosted URL (auth required);
  `POST /webhooks/stripe` verifies the `t=..,v1=..` HMAC signature (rejecting
  forged/stale/multi-secret-rotation cases) and persists the customer id on
  `checkout.session.completed`.

### The WebRTC data path (Phases 1–3, the product core)
- `transport::PeerSession` wraps `RTCPeerConnection` with a bidirectional
  `control` DataChannel carrying `protocol::ChannelMessage`. **Unit test**: two
  sessions negotiate directly (trickle ICE, loopback) and an `InputEvent`
  round-trips.
- **End-to-end through the live relay** (server integration test): a client
  offerer and agent answerer negotiate entirely via the server's WebSocket
  relay and a client→agent `InputEvent` is delivered. No GPU/display — webrtc-rs
  negotiates in software over loopback (non-trickle ICE).
- The **agent** uses `PeerSession::answerer`, keeps a single persistent server
  WebSocket, and injects received input on macOS via CoreGraphics (mouse move/
  button/scroll + a HID→keycode map for common keys).
- The **client** uses `PeerSession::offerer` (`signaling::establish_session`) and
  forwards egui pointer/button/scroll/key events as `InputEvent`s.

### Shared crates
- `protocol`: bincode round-trip tests for every `InputEvent` / `ControlMessage`
  variant; garbage-decode rejection.
- `transport`: builds a real `RTCPeerConnection`, creates a data channel, and
  produces a valid SDP offer; plus the `PeerSession` round-trip above.

### Client (`apps/client`) — non-UI logic
- `ApiClient` (reqwest): `get_me` / `get_machines` / `connect_machine` parse the
  server's real JSON contract (verified against an in-process mock), send the
  bearer token, and error cleanly without a token. Tested.
- `auth::LoopbackReceiver`: real one-shot loopback HTTP server that captures the
  OAuth callback; handles partial reads and surfaces `error` params. Tested with
  a real TCP round-trip.
- `auth::TokenStore`: OS-keychain-backed (via `keyring`), tested through the
  crate's mock backend.
- The egui app is wired with a non-blocking channel pattern (background Tokio
  tasks + per-frame `try_recv`); auto-resumes a stored session on launch and
  establishes the WebRTC session on connect.

### Bug fixes made along the way
- Client event loop: the `RedrawRequested` arm was unreachable, so **the app
  never rendered**; merged into the main `WindowEvent` arm and added the missing
  egui texture/buffer uploads.
- Client login used `Handle::block_on` inside the winit event-loop thread, which
  **panics** inside a Tokio context; `LoopbackReceiver::bind()` is now synchronous.
- **Agent WebSocket**: previously opened a fresh connection per message, which
  (with the relay's per-connection registration) flapped the machine offline;
  rewritten as one persistent connection with reader/writer tasks aborted on drop.
- **Connect race**: `/connect` now gates on the in-memory relay, not the
  eventually-consistent DB `online` flag, fixing a reconnect race that left a
  live machine marked offline.

## Not verifiable in this environment (and why)

| Area | Why it can't be verified here |
|---|---|
| **Windows agent**: DXGI capture, NVENC / Media Foundation encode, WASAPI audio, SendInput injection, Windows service | Requires Windows + specific GPU hardware. Code is `cfg`-gated out on macOS and remains stubbed (`anyhow::bail!`). This is Phase 1. |
| **Screen-capture → video track** (both platforms) | The signaling, control channel, and input path are done and tested; adding a *video* track needs the capture+encode pipeline running against a real display/GPU (macOS ScreenCaptureKit/VideoToolbox needs Screen Recording permission). |
| **macOS input injection at runtime** | The CoreGraphics code compiles and is logically correct, but actually moving the cursor/keys needs Accessibility permission and a session — can't be exercised headlessly. |
| **Live WorkOS AuthKit / live Stripe** | Our half of both protocols is implemented and tested against local mocks; the hosted WorkOS login UI and a real Stripe account are external. |
| **Client video pipeline** (hardware decode → wgpu render, audio playback) | Needs a GPU/display and a live media peer. |
| **Real P2P over Tailscale, latency/FPS NFRs** | Needs two machines on a tailnet and real media. |

## Concrete next steps (in dependency order)

1. macOS agent: feed the existing ScreenCaptureKit/VideoToolbox H.265 output into
   a WebRTC video track added to `PeerSession::peer_connection()` before answering.
2. Client video: hardware-decode the incoming track (VideoToolbox) and render
   YUV→RGB in wgpu in the connected view.
3. Windows agent (Phase 1): DXGI + NVENC/MF + WASAPI + SendInput + service.
4. Billing polish (Phase 4): usage tracking / connection-minute metering (S11).
