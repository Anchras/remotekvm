# MVP Specification

## What is the MVP?

The Minimum Viable Product is a **functioning remote desktop SaaS** with:
- A centrally-hosted coordination server
- Windows agents that run as a service and stream the desktop
- A native client app that can log in and connect to remote machines
- Audio passthrough
- Multi-tenant support (B2B teams + B2C individuals)
- Stripe billing

**What the MVP is NOT:**
- Not a gaming remote desktop (no adaptive bitrate for games, no controller support)
- Not a pre-login remote access tool (Windows lock screen is not accessible)
- Not a web client (native desktop app only)
- Not a mobile client

---

## Use Cases

### B2C: Individual Developer
> Sarah has a powerful workstation at home and a laptop for travel. She installs the RemoteKVM agent on her workstation, logs into the client on her laptop, and gets full remote access with audio for coding and meetings.

### B2B: Distributed Team
> Acme Corp has 50 engineers with high-end workstations in the office. They create a RemoteKVM team, invite all engineers, and everyone can remotely access their office machine from home with full audio and low-latency video.

---

## Functional Requirements

### Server

| ID | Requirement | Priority |
|----|-------------|----------|
| S1 | Support OAuth2 login via Google, GitHub, and Microsoft | P0 |
| S2 | Issue JWT sessions with 24h expiry | P0 |
| S3 | Allow agents to register with a machine token | P0 |
| S4 | Relay WebRTC signaling (SDP + ICE) between client and agent | P0 |
| S5 | Track machine online/offline status via heartbeats | P0 |
| S6 | REST API for machine listing and connection requests | P0 |
| S7 | Support team creation and member invites | P1 |
| S8 | Stripe Checkout for subscription management | P1 |
| S9 | Handle Stripe webhooks for subscription events | P1 |
| S10 | Usage tracking (connection minutes) for billing | P2 |

### Agent (Windows)

| ID | Requirement | Priority |
|----|-------------|----------|
| A1 | Run as a Windows service with a session subprocess | P0 |
| A2 | Capture desktop via DXGI Desktop Duplication | P0 |
| A3 | Encode video with NVENC (primary) or Media Foundation (fallback) | P0 |
| A4 | Capture system audio via WASAPI loopback | P0 |
| A5 | Encode audio with Opus | P0 |
| A6 | Establish WebRTC peer connection with client | P0 |
| A7 | Accept input events via DataChannel and inject with SendInput | P0 |
| A8 | Handle connection requests from server | P0 |
| A9 | Support dynamic bitrate adjustment via ControlMessage | P1 |
| A10 | Support explicit keyframe requests | P1 |
| A11 | Graceful degradation on permission denial (screen recording) | P1 |

### Client

| ID | Requirement | Priority |
|----|-------------|----------|
| C1 | Native desktop app (Windows and macOS) | P0 |
| C2 | OAuth2 login screen | P0 |
| C3 | Dashboard showing user's machines with online status | P0 |
| C4 | Click-to-connect flow | P0 |
| C5 | Hardware video decode (Media Foundation on Win, VideoToolbox on macOS) | P0 |
| C6 | Audio playback | P0 |
| C7 | Mouse and keyboard input forwarding | P0 |
| C8 | Scroll wheel support | P0 |
| C9 | Connection quality indicator (bitrate, latency, packet loss) | P1 |
| C10 | Fullscreen mode | P1 |
| C11 | Clipboard sync (host → client) | P2 |

---

## Non-Functional Requirements

| Requirement | Target |
|-------------|--------|
| Video latency (encode + network + decode) | < 50ms on same tailnet |
| Audio latency | < 100ms |
| Video quality | 1080p @ 60fps, H.264 |
| Startup time (client → connected) | < 5 seconds |
| Agent CPU usage (idle) | < 1% |
| Agent CPU usage (streaming 1080p60) | < 5% (NVENC) |
| Client CPU usage (decoding 1080p60) | < 10% |
| Max concurrent connections per machine | 1 (MVP) |
| Supported platforms (agent) | Windows 10/11 |
| Supported platforms (client) | Windows 10/11, macOS 12+ |

---

## Out of Scope for MVP

- Linux agent or client
- Web client
- Mobile client
- Multi-monitor support
- Pre-login / lock screen access
- File transfer
- Session recording
- Administrative dashboard
- On-premise deployment
- Load balancing / horizontal scaling of signaling server
- Automatic failover / high availability

---

## Acceptance Criteria

A user can:
1. Sign up via OAuth2 on the client app
2. Install the agent on their Windows machine
3. See their machine appear online in the client dashboard
4. Click "Connect" and see their remote desktop with audio within 5 seconds
5. Use mouse and keyboard as if locally connected
6. Disconnect and reconnect without issues
7. Invite team members (B2B) and share machine access
8. Subscribe to a paid plan via Stripe

---

## Technical Decisions

| Decision | Rationale |
|----------|-----------|
| Rust end-to-end | Consistency, performance, shared crates |
| Axum for server | Best async Rust HTTP framework |
| wgpu + egui for client | Full control over video render, cross-platform |
| NVENC primary encode | Lowest latency, best quality for workstations |
| Tailscale for networking | Zero-config mesh, built-in encryption, no TURN needed |
| Windows service for agent | True unattended access |
| Bincode for protocol | Fast, compact, Rust-native serialization |
| WebRTC for transport | Standard P2P, NAT traversal, DTLS-SRTP encryption |
| PostgreSQL for data | Robust, well-supported, sqlx for type safety |
| Redis for sessions | Fast ephemeral state, signaling pub/sub |

---

## License
AGPL-3.0-or-later
