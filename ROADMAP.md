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

- [ ] Project restructure: `apps/host` → `apps/agent`, add `apps/server`
- [ ] Axum HTTP server with health endpoint
- [ ] PostgreSQL + sqlx migrations
- [ ] OAuth2 (Google, GitHub, Microsoft) authentication
- [ ] JWT session management
- [ ] WebSocket signaling relay for WebRTC
- [ ] REST API: machines list, connect request
- [ ] Agent registration with tokens
- [ ] Docker Compose for local dev

### Phase 1: Windows Agent Core (6–8 weeks)
**Goal**: A Windows service that captures the desktop, encodes it, and streams over WebRTC.

- [ ] Windows service architecture (service controller + session agent)
- [ ] DXGI Desktop Duplication capture
- [ ] NVENC hardware encoding (primary)
- [ ] Media Foundation fallback (AMD/Intel)
- [ ] WASAPI loopback audio capture → Opus
- [ ] WebRTC peer connection (H.264 + Opus + DataChannel)
- [ ] SendInput mouse/keyboard injection
- [ ] Control message handling (bitrate, keyframe requests)

### Phase 2: Native Client + Decode/Render (6–8 weeks)
**Goal**: A polished native app that can log in, see machines, and remote-control them.

- [ ] winit + wgpu + egui application framework
- [ ] OAuth2 login flow
- [ ] Machine dashboard / list view
- [ ] Connection flow (request → WebRTC establishment)
- [ ] Hardware video decode (Media Foundation on Win, VideoToolbox on macOS)
- [ ] YUV→RGB wgpu shader pipeline
- [ ] Audio playback (cpal + Opus decode)
- [ ] Input capture (winit → InputEvent → DataChannel)
- [ ] Custom egui theming for professional look

### Phase 3: macOS Agent Polish + Cross-Platform (3–4 weeks)
**Goal**: Refactor existing macOS code into agent architecture; client works on both platforms.

- [ ] macOS agent as launchd background job
- [ ] Port existing ScreenCaptureKit + VideoToolbox code
- [ ] macOS input injection (CGEventPost)
- [ ] macOS client hardware decode (VideoToolbox)
- [ ] Linux agent (PipeWire + VAAPI) — post-MVP

### Phase 4: Billing + Multi-Tenancy + Deploy (3–4 weeks)
**Goal**: Stripe integration, team management, and production deployment.

- [ ] Stripe Checkout for B2C (pay-per-machine)
- [ ] Stripe Checkout for B2B (per-seat team plans)
- [ ] Team creation and invitation flow
- [ ] Role-based access control (owner, admin, member)
- [ ] Machine sharing within teams
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
