# RemoteKVM Architecture

## Overview

RemoteKVM is a centrally-hosted, multi-tenant SaaS for remote desktop access. It consists of three main components:

1. **SaaS Server** — Coordination, authentication, and WebRTC signaling
2. **Agent** — Runs on the host machine, captures screen/audio, streams over WebRTC
3. **Client** — Native desktop app that connects to agents and renders the remote desktop

All three are written in Rust for consistency, performance, and code sharing.

---

## System Architecture

```
┌─────────────────┐         ┌─────────────────────┐         ┌──────────────────┐
│  Native Client  │  WSS    │   SaaS Coordinator  │  WSS    │   Host Agent     │
│  (Rust/wgpu)    │◄───────►│   (Auth/Signal/API) │◄───────►│  (Win/macOS)     │
│  - egui UI      │         │   - PostgreSQL      │         │  - Capture       │
│  - HW Decode    │  WebRTC │   - Redis           │         │  - HW Encode     │
│  - Input Fwd    │  (P2P)  │   - OAuth2/JWT      │         │  - Input Inject  │
└─────────────────┘         └─────────────────────┘         └──────────────────┘
         │                           │                            │
         └────────── Tailnet ─────────┴────────────────────────────┘
```

### Network Flow

1. Agent connects outbound to SaaS server via WebSocket (WSS) with a registration token
2. Client logs in via OAuth2, gets a JWT session
3. Client connects to SaaS server via WebSocket (WSS) with JWT
4. Client requests connection to a machine
5. Server relays WebRTC signaling (SDP offer/answer, ICE candidates) between client and agent
6. WebRTC data flows directly P2P over Tailscale IPs

---

## Component Details

### SaaS Server (`apps/server`)

**Stack:** Axum, PostgreSQL, Redis, sqlx, tokio

**Responsibilities:**
- OAuth2 authentication (Google, GitHub, Microsoft)
- JWT session management
- User and team management
- Machine registration and discovery
- WebRTC signaling relay
- Stripe billing webhooks
- REST API for dashboard data

**Key APIs:**
- `POST /auth/callback/{provider}` — OAuth2 callback
- `GET /api/me` — Current user profile
- `GET /api/machines` — List user's machines
- `POST /api/machines/{id}/connect` — Request connection
- `GET /api/teams` — List user's teams
- `POST /api/teams` — Create team

**WebSocket Protocol:**
- Agents: `wss://server/agent` with `Authorization: Bearer <registration_token>`
- Clients: `wss://server/client` with `Authorization: Bearer <jwt_session>`
- Message types: `Hello`, `MachineList`, `ConnectRequest`, `SignalingOffer`, `SignalingAnswer`, `IceCandidate`, `Heartbeat`

### Agent (`apps/agent`)

**Stack:** Rust, webrtc-rs, windows-service (Windows), objc2 (macOS)

**Architecture:**

On Windows, the agent uses a two-process architecture:

```
┌─────────────────────────────────────────────┐
│          Windows Service (Session 0)        │
│  - Persistent connection to SaaS server     │
│  - Monitors active user sessions            │
│  - Spawns/kills agent process per session   │
│  - Handles machine registration, heartbeats  │
└────────────────┬────────────────────────────┘
                 │ IPC (named pipes)
                 ▼
┌─────────────────────────────────────────────┐
│       Session Agent (Active User Session)     │
│  - DXGI Desktop Duplication                   │
│  - NVENC / Media Foundation encode            │
│  - WASAPI audio capture                       │
│  - WebRTC peer connection                     │
│  - SendInput injection                        │
└─────────────────────────────────────────────┘
```

On macOS, the agent runs as a `launchd` background job (single process):

```
┌─────────────────────────────────────────────┐
│       macOS Agent (launchd)                 │
│  - ScreenCaptureKit capture                 │
│  - VideoToolbox HEVC encode                 │
│  - Core Audio capture                       │
│  - WebRTC peer connection                     │
│  - CGEventPost injection                    │
└─────────────────────────────────────────────┘
```

**Encode Priority:**
1. NVIDIA NVENC (fastest, lowest latency)
2. Media Foundation (generic Windows)
3. VideoToolbox (macOS)

### Client (`apps/client`)

**Stack:** Rust, winit, wgpu, egui, webrtc-rs, cpal

**Responsibilities:**
- OAuth2 login flow
- Machine dashboard (list of user's machines with online status)
- WebRTC connection establishment
- Hardware video decode
- YUV→RGB rendering via wgpu
- Audio playback
- Input capture and forwarding

**UI Flow:**
1. Login screen (OAuth2 provider selection)
2. Dashboard (grid/list of machines, online/offline status)
3. Connection view (video stream + input capture)

---

## Data Flow

### Screen Capture → Encode → Stream

```
Host Machine:
  Capture API (DXGI/SCK) → CVPixelBuffer/D3D11Texture
    → Hardware Encoder (NVENC/VT/AMF)
      → H.264/HEVC NAL units
        → WebRTC Video Track
          → RTP → SRTP → UDP (P2P over Tailscale)
            → Client WebRTC → Hardware Decoder → wgpu Texture → Display
```

### Input Events

```
Client:
  winit Input Event
    → InputEvent (bincode)
      → WebRTC DataChannel
        → RTP → SRTP → UDP (P2P over Tailscale)
          → Agent DataChannel → InputEvent (bincode)
            → Platform Input API (SendInput/CGEventPost)
```

### Audio

```
Host:
  WASAPI/CoreAudio Loopback Capture
    → Opus Encode
      → WebRTC Audio Track
        → RTP → SRTP → UDP (P2P over Tailscale)
          → Client WebRTC → Opus Decode
            → cpal Playback
```

---

## Shared Crates

### `crates/protocol`

Core protocol definitions shared across all components:

- `InputEvent` — Mouse move, mouse button, scroll, key down/up
- `ControlMessage` — Hello, bitrate set, keyframe request, host info
- `WireError` — Serialization/deserialization errors
- `encode`/`decode` — Bincode helpers

### `crates/transport`

WebRTC transport wrapper:

- `new_peer_connection()` — Create PeerConnection with low-latency defaults
- No ICE servers (Tailscale provides direct connectivity)
- Configured for screen content (not camera)

---

## Database Schema

### Users
```sql
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email TEXT UNIQUE NOT NULL,
    name TEXT,
    avatar_url TEXT,
    stripe_customer_id TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);
```

### OAuth Accounts
```sql
CREATE TABLE oauth_accounts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL, -- 'google', 'github', 'microsoft'
    provider_user_id TEXT NOT NULL,
    UNIQUE(provider, provider_user_id)
);
```

### Teams
```sql
CREATE TABLE teams (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    slug TEXT UNIQUE NOT NULL,
    owner_id UUID NOT NULL REFERENCES users(id),
    stripe_subscription_id TEXT,
    plan TEXT NOT NULL DEFAULT 'free', -- 'free', 'pro', 'enterprise'
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### Team Members
```sql
CREATE TABLE team_members (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id UUID NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL DEFAULT 'member', -- 'owner', 'admin', 'member'
    joined_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(team_id, user_id)
);
```

### Machines
```sql
CREATE TABLE machines (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    team_id UUID REFERENCES teams(id) ON DELETE SET NULL,
    name TEXT NOT NULL,
    hostname TEXT NOT NULL,
    tailscale_ip INET,
    platform TEXT NOT NULL, -- 'windows', 'macos', 'linux'
    registration_token_hash TEXT NOT NULL,
    online BOOLEAN NOT NULL DEFAULT FALSE,
    last_seen TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### Sessions
```sql
CREATE TABLE sessions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    machine_id UUID NOT NULL REFERENCES machines(id) ON DELETE CASCADE,
    started_at TIMESTAMPTZ DEFAULT NOW(),
    ended_at TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'active' -- 'active', 'ended', 'error'
);
```

---

## Security Considerations

1. **Agent authentication**: Registration tokens (hashed in DB) used for initial agent connection
2. **Client authentication**: OAuth2 + JWT sessions with short expiry
3. **WebRTC security**: DTLS-SRTP for encryption in transit; P2P over Tailscale adds another layer
4. **Tailscale**: Leverages WireGuard for network-level encryption
5. **Input injection**: Agent only accepts input from authenticated, active WebRTC sessions
6. **Screen capture**: Requires OS-level permissions; agent must handle permission denial gracefully

---

## License
AGPL-3.0-or-later
