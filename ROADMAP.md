# RemoteKVM Roadmap

## Mission
Build a centrally-hosted, multi-tenant SaaS for remote workstation access — a Parsec alternative optimized for productivity, not gaming.

## Core Principles
1. **Performance first**: NVENC where available, never default to the slow path
2. **Native everywhere**: Rust end-to-end for consistency and performance
3. **Tailscale-native**: Leverage Tailscale for mesh networking and identity
4. **Windows service**: True unattended access, not just a tray app
5. **Audio is essential**: Not a nice-to-have, it's a core feature
6. **B2B + B2C**: Teams and individuals, with Stripe billing from day one

---

## Phases

### Phase 0: SaaS Server Foundation (4–6 weeks)
**Goal**: A working coordination server that agents can register with and clients can log into.

_Status: complete and locally verified (27 automated tests). See [STATUS.md](STATUS.md)._

- [x] Project restructure: `apps/host` → `apps/agent`, add `apps/server`
- [x] Axum HTTP server with health endpoint
- [x] PostgreSQL + sqlx migrations
- [x] OAuth2 authentication via WorkOS AuthKit (login-init + signed-state loopback callback; server flow tested against a mock WorkOS — live AuthKit needs real credentials)
- [x] JWT session management
- [x] WebSocket signaling relay for WebRTC (agent↔client offer/answer/ICE relay, end-to-end tested)
- [x] REST API: machines list, connect request
- [x] Agent registration with tokens (hashed, rotatable)
- [x] Docker Compose for local dev (Postgres + Redis; server still runs natively — server image `Dockerfile` is a remaining TODO)

### Phase 1: Windows Agent Core (6–8 weeks)
**Goal**: A Windows service that captures the desktop, encodes it, and streams over WebRTC.

_Status: cross-platform WebRTC + control channel + signaling are done and tested
(`transport::PeerSession`); Windows-native capture/encode/audio/service remain
hardware-gated. See [STATUS.md](STATUS.md)._

- [ ] Windows service architecture (service controller + session agent)
- [ ] DXGI Desktop Duplication capture
- [ ] NVENC hardware encoding (primary)
- [ ] Media Foundation fallback (AMD/Intel)
- [ ] WASAPI loopback audio capture → Opus
- [x] WebRTC peer connection (DataChannel done + tested end-to-end through the relay; H.264 video track + Opus audio pending the capture pipeline)
- [~] SendInput mouse/keyboard injection (macOS CGEventPost done; Windows SendInput pending)
- [~] Control message handling (channel carries `ControlMessage`; bitrate/keyframe handlers pending the encoder)

### Phase 2: Native Client + Decode/Render (6–8 weeks)
**Goal**: A polished native app that can log in, see machines, and remote-control them.

_Status: app shell + non-UI logic done and unit-tested; media pipeline pending. See [STATUS.md](STATUS.md)._

- [x] winit + wgpu + egui application framework (render loop fixed; egui buffers uploaded)
- [x] OAuth2 login flow — loopback callback receiver + AuthKit URL + Keychain token store, all tested (browser leg needs live WorkOS)
- [x] Machine dashboard / list view (backed by the real REST API client)
- [x] Connection flow (request → WebRTC establishment) — `signaling::establish_session` negotiates the offer/answer through the relay and opens the control channel
- [ ] Hardware video decode (Media Foundation on Win, VideoToolbox on macOS)
- [ ] YUV→RGB wgpu shader pipeline
- [ ] Audio playback (cpal + Opus decode)
- [x] Input capture (egui events → InputEvent → DataChannel)
- [x] Custom egui theming (dark slate/cyan)

### Phase 3: macOS Agent Polish + Cross-Platform (3–4 weeks)
**Goal**: Refactor existing macOS code into agent architecture; client works on both platforms.

- [ ] macOS agent as launchd background job
- [~] Port existing ScreenCaptureKit + VideoToolbox code (capture/encode code exists; not yet fed into the WebRTC video track)
- [x] macOS input injection (CGEventPost) — mouse move/button/scroll + common keys
- [ ] macOS client hardware decode (VideoToolbox)
- [ ] Linux agent (PipeWire + VAAPI) — post-MVP

### Phase 4: Billing + Multi-Tenancy + Deploy (3–4 weeks)
**Goal**: Stripe integration, team management, and production deployment.

_Status: Stripe Checkout + signed webhook implemented and tested against a mock.
Org management/RBAC are delegated to WorkOS (synced on login). See [STATUS.md](STATUS.md)._

- [x] Stripe Checkout (subscription) — `POST /api/billing/checkout`, tested against a mock
- [x] Stripe webhook handling — signature-verified `POST /webhooks/stripe`, persists customer id
- [x] Team creation and invitation flow — delegated to WorkOS Organizations (synced on login)
- [x] Role-based access control (owner, admin, member) — WorkOS roles synced into `organization_members`
- [x] Machine sharing within teams (org-scoped machine queries)
- [ ] Fly.io / Hetzner deployment
- [ ] TLS, reverse proxy, basic monitoring
- [ ] Usage tracking and quotas

---

## Post-MVP

- [ ] Pre-login capture (kernel display driver)
- [ ] Multi-monitor support
- [ ] Clipboard sync
- [ ] File transfer
- [ ] Session recording
- [ ] Linux agent support
- [ ] Web client (WebRTC in browser)
- [ ] Mobile client (iOS/Android)
- [ ] Admin dashboard
- [ ] On-premise deployment option

---

## Timeline

| Phase | Duration | Cumulative |
|-------|----------|------------|
| Phase 0 | 4–6 weeks | 4–6 weeks |
| Phase 1 | 6–8 weeks | 10–14 weeks |
| Phase 2 | 6–8 weeks | 16–22 weeks |
| Phase 3 | 3–4 weeks | 19–26 weeks |
| Phase 4 | 3–4 weeks | 22–30 weeks |

**Total MVP: ~5–7 months** for a team of 2–3 engineers.

---

## License
AGPL-3.0-or-later
