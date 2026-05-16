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
┌─────────────────┐   AuthKit/SSO   ┌─────────────┐         ┌──────────────────┐
│  Native Client  │◄───────────────►│   WorkOS    │   WSS   │   SaaS Server    │
│  (Rust/wgpu)    │                 │             │◄───────►│   (Axum + sqlx)  │
│  - egui UI      │                 │             │         │   - Machines     │
│  - HW Decode    │                 │             │         │   - WebRTC Signal│
│  - Input Fwd    │                 │             │         │   - Stripe       │
└─────────────────┘                 └─────────────┘         └────────┬─────────┘
         │                                                           │  WSS
         │                              Tailnet                      ▼
         │    ┌────────────────────────────────────────────────┐  ┌─────────────┐
         └───►│               P2P WebRTC over Tailscale         │  │ Host Agent  │
              │         Video/Audio/Input DataChannel           │  │ (Win/macOS) │
              └────────────────────────────────────────────────┘  └─────────────┘
```

### Network Flow

1. **Client** opens browser to WorkOS AuthKit (hosted login UI)
2. **WorkOS** authenticates user (social, passwordless, or enterprise SSO) and redirects to our server with an authorization code
3. **Server** exchanges code with WorkOS for user profile, issues its own JWT session
4. **Agent** connects outbound to SaaS server via WebSocket with a registration token
5. **Client** connects to SaaS server via WebSocket with JWT
6. Server relays WebRTC signaling (SDP offer/answer, ICE candidates) between client and agent
7. WebRTC data flows directly P2P over Tailscale IPs

---

## Component Details

### SaaS Server (`apps/server`)

**Stack:** Axum, PostgreSQL, Redis, sqlx, tokio, WorkOS API

**Responsibilities:**
- **WorkOS integration** — User authentication via AuthKit, multi-tenant organizations
- **JWT session management** — Own JWTs (24h expiry) + periodic WorkOS validation
- **Machine registration and discovery** — Agents register, clients discover their machines
- **WebRTC signaling relay** — SDP offer/answer, ICE candidates
- **Stripe billing webhooks** — Subscription management
- **REST API** — Dashboard data (machines, sessions, billing)

**Key APIs:**
- `POST /auth/workos/callback` — WorkOS AuthKit callback, exchanges code for JWT
- `GET /api/me` — Current user + WorkOS organizations
- `GET /api/machines` — List user's/organization's machines
- `POST /api/machines/{id}/connect` — Request connection
- `GET /api/billing/subscription` — Subscription details
- `POST /api/billing/checkout` — Stripe Checkout session

**What WorkOS Handles (we do NOT build):**
- OAuth2 provider integrations (Google, Microsoft, GitHub, etc.)
- Organization (team) creation and member management
- Role-based access control (RBAC)
- Enterprise SSO (SAML) configuration — via WorkOS Admin Portal
- Directory Sync (SCIM) — post-MVP
- Admin Portal for IT self-service — post-MVP

**WebSocket Protocol:**
- Agents: `wss://server/agent` with `Authorization: Bearer <registration_token>`
- Clients: `wss://server/client` with `Authorization: Bearer <jwt_session>`
- Message types: `Hello`, `MachineList`, `ConnectRequest`, `SignalingOffer`, `SignalingAnswer`, `IceCandidate`, `Heartbeat`

### WorkOS

**Products Used in MVP:**
- **AuthKit** — Hosted login UI with social login, passwordless, MFA
- **Organizations** — Multi-tenant auth, org membership, role-based access

**Products Used Post-MVP:**
- **SSO** — SAML/OIDC enterprise authentication (Okta, Azure AD, etc.)
- **Admin Portal** — IT self-service SSO configuration
- **Directory Sync (SCIM)** — Automatic user provisioning from identity providers

**Data Sync Strategy:**
- Our `users` and `organizations` tables are a mirror of WorkOS data
- Sync on login (upsert user + orgs from WorkOS profile)
- For MVP: sync-on-login is sufficient; webhooks for live sync post-MVP

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
- WorkOS AuthKit login flow (opens browser, receives callback)
- Machine dashboard (list of user's/org's machines with online status)
- WebRTC connection establishment
- Hardware video decode
- YUV→RGB rendering via wgpu
- Audio playback
- Input capture and forwarding

**UI Flow:**
1. Login screen (opens WorkOS AuthKit in browser)
2. Dashboard (grid/list of machines, online/offline status)
3. Connection view (video stream + input capture)

---

## Data Flow

### Authentication Flow

```
Client:
  Open browser → WorkOS AuthKit URL (with our redirect_uri)
    → User authenticates (Google/Microsoft/Passwordless)
      → WorkOS redirects to our server with code
        → Server: exchange code with WorkOS API
          → WorkOS returns user profile + organizations
            → Server upserts user + orgs in PostgreSQL
              → Server issues JWT session token
                → Redirect back to client (deep link: remotekvm://auth?token=...)
                  → Client stores JWT for API access
```

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

### Users (Mirror of WorkOS)

```sql
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workos_user_id TEXT UNIQUE NOT NULL,
    email TEXT UNIQUE NOT NULL,
    first_name TEXT,
    last_name TEXT,
    avatar_url TEXT,
    stripe_customer_id TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);
```

### Organizations (Mirror of WorkOS)

```sql
CREATE TABLE organizations (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    workos_org_id TEXT UNIQUE NOT NULL,
    name TEXT NOT NULL,
    slug TEXT UNIQUE NOT NULL,
    stripe_subscription_id TEXT,
    plan TEXT NOT NULL DEFAULT 'free',
    created_at TIMESTAMPTZ DEFAULT NOW()
);
```

### Organization Members (Mirror of WorkOS)

```sql
CREATE TABLE organization_members (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    organization_id UUID NOT NULL REFERENCES organizations(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role TEXT NOT NULL DEFAULT 'member',
    joined_at TIMESTAMPTZ DEFAULT NOW(),
    UNIQUE(organization_id, user_id)
);
```

### Machines

```sql
CREATE TABLE machines (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID REFERENCES users(id) ON DELETE SET NULL,
    organization_id UUID REFERENCES organizations(id) ON DELETE SET NULL,
    name TEXT NOT NULL,
    hostname TEXT NOT NULL,
    tailscale_ip INET,
    platform TEXT NOT NULL,
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
    status TEXT NOT NULL DEFAULT 'active'
);
```

**Dropped from previous design:**
- `oauth_accounts` — WorkOS handles all OAuth provider integrations
- `teams` / `team_members` — Replaced by `organizations` / `organization_members` (WorkOS-managed)

---

## Security Considerations

1. **Agent authentication**: Registration tokens (hashed in DB) used for initial agent connection
2. **Client authentication**: WorkOS AuthKit + our own JWT sessions with short expiry
3. **WebRTC security**: DTLS-SRTP for encryption in transit; P2P over Tailscale adds another layer
4. **Tailscale**: Leverages WireGuard for network-level encryption
5. **Input injection**: Agent only accepts input from authenticated, active WebRTC sessions
6. **Screen capture**: Requires OS-level permissions; agent must handle permission denial gracefully
7. **WorkOS API key**: Stored securely (environment variable only, never in code); server-side only

---

## License
AGPL-3.0-or-later
