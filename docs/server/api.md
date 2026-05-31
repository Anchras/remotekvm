# Server API Specification

## Authentication

All API requests (except the WorkOS callback) require a Bearer JWT token in the `Authorization` header:

```
Authorization: Bearer <jwt_token>
```

The JWT is issued by our server after successful WorkOS AuthKit authentication.

## Endpoints

### Health

#### `GET /health`

Returns server health status.

**Response:**
```json
{
  "status": "ok",
  "version": "0.1.0",
  "timestamp": "2026-05-16T12:00:00Z"
}
```

### Authentication (WorkOS AuthKit)

The client initiates login by opening WorkOS AuthKit in a browser:

```
https://auth.workos.com/authenticate?client_id=<workos_client_id>&redirect_uri=<our_callback>&response_type=code&...`
```

After authentication, WorkOS redirects to our callback endpoint.

#### `GET /auth/workos/callback?code=...`

WorkOS AuthKit callback endpoint. Exchanges code for profile via WorkOS API, creates/updates user and organizations, issues JWT session.

**Response:**
```json
{
  "token": "<jwt_token>",
  "user": {
    "id": "uuid",
    "workos_user_id": "user_xxxxxxxx",
    "email": "user@example.com",
    "first_name": "User",
    "last_name": "Name",
    "avatar_url": "https://...",
    "organizations": [
      {
        "id": "uuid",
        "workos_org_id": "org_xxxxxxxx",
        "name": "Acme Corp",
        "slug": "acme-corp",
        "role": "member"
      }
    ]
  }
}
```

**Error Responses:**
- `400` — Missing or invalid code
- `401` — WorkOS code exchange failed
- `500` — Internal error during profile sync

### User

#### `GET /api/me`

Returns the current authenticated user with WorkOS organizations.

**Response:**
```json
{
  "id": "uuid",
  "workos_user_id": "user_xxxxxxxx",
  "email": "user@example.com",
  "first_name": "User",
  "last_name": "Name",
  "avatar_url": "https://...",
  "organizations": [
    {
      "id": "uuid",
      "workos_org_id": "org_xxxxxxxx",
      "name": "Acme Corp",
      "slug": "acme-corp",
      "role": "member"
    }
  ]
}
```

### Machines

#### `GET /api/machines`

List all machines accessible to the current user:
- Personal machines (where `user_id` matches)
- Organization machines (where `organization_id` matches user's orgs)

**Response:**
```json
{
  "machines": [
    {
      "id": "uuid",
      "name": "Workstation",
      "hostname": "DESKTOP-ABC123",
      "tailscale_ip": "100.x.x.x",
      "platform": "windows",
      "online": true,
      "last_seen": "2026-05-16T11:55:00Z",
      "owner": {
        "id": "uuid",
        "name": "User Name"
      },
      "organization": {
        "id": "uuid",
        "name": "Acme Corp"
      }
    }
  ]
}
```

#### `POST /api/machines`

Register a new machine. Called by the agent during first setup.

**Request:**
```json
{
  "name": "Workstation",
  "hostname": "DESKTOP-ABC123",
  "platform": "windows",
  "tailscale_ip": "100.x.x.x"
}
```

**Response:**
```json
{
  "id": "uuid",
  "registration_token": "rkvm_xxxxxxxxxxxx"
}
```

#### `GET /api/machines/{id}`

Get details for a specific machine.

#### `POST /api/machines/{id}/connect`

Request a connection to a machine. The server will forward this to the agent.

**Response:**
```json
{
  "session_id": "uuid",
  "status": "pending"
}
```

#### `DELETE /api/machines/{id}`

Delete a machine registration.

### Organizations

**Note:** Organization CRUD is managed by WorkOS. Our server mirrors the data.

#### `GET /api/organizations`

List organizations the user is a member of (from our DB mirror of WorkOS).

**Response:**
```json
{
  "organizations": [
    {
      "id": "uuid",
      "workos_org_id": "org_xxxxxxxx",
      "name": "Acme Corp",
      "slug": "acme-corp",
      "role": "member",
      "plan": "pro"
    }
  ]
}
```

#### `GET /api/organizations/{id}/members`

List members of an organization.

**Response:**
```json
{
  "members": [
    {
      "user_id": "uuid",
      "email": "member@example.com",
      "name": "Member Name",
      "role": "admin"
    }
  ]
}
```

### Billing

#### `GET /api/billing/subscription`

Get current subscription details.

#### `POST /api/billing/checkout`

Create a Stripe Checkout session.

**Request:**
```json
{
  "plan": "pro",
  "billing_period": "monthly"
}
```

**Response:**
```json
{
  "checkout_url": "https://checkout.stripe.com/..."
}
```

#### `POST /api/billing/portal`

Create a Stripe Customer Portal session.

**Response:**
```json
{
  "portal_url": "https://billing.stripe.com/..."
}
```

### Webhooks

#### `POST /webhooks/stripe`

Stripe webhook endpoint.

#### `POST /webhooks/workos` (Post-MVP)

WorkOS webhooks for live sync of user/org changes.

---

## WebSocket Protocol

### Agent Connection

**URL:** `wss://server/agent`

**Headers:**
```
Authorization: Bearer <registration_token>
```

**Messages (Server → Agent):**

#### `ConnectRequest`
```json
{
  "type": "connect_request",
  "session_id": "uuid",
  "client_id": "uuid",
  "offer": "<sdp_offer>"
}
```

#### `Heartbeat`
```json
{
  "type": "heartbeat"
}
```

**Messages (Agent → Server):**

#### `Hello`
```json
{
  "type": "hello",
  "machine_id": "uuid",
  "version": "0.1.0"
}
```

#### `SignalingAnswer`
```json
{
  "type": "signaling_answer",
  "session_id": "uuid",
  "answer": "<sdp_answer>"
}
```

#### `IceCandidate`
```json
{
  "type": "ice_candidate",
  "session_id": "uuid",
  "candidate": "<ice_candidate>"
}
```

#### `MachineStatus`
```json
{
  "type": "machine_status",
  "online": true,
  "tailscale_ip": "100.x.x.x"
}
```

### Client Connection

**URL:** `wss://server/client`

**Headers:**
```
Authorization: Bearer <jwt_token>
```

**Messages (Server → Client):**

#### `MachineList`
```json
{
  "type": "machine_list",
  "machines": [...]
}
```

#### `ConnectResponse`
```json
{
  "type": "connect_response",
  "session_id": "uuid",
  "status": "accepted",
  "answer": "<sdp_answer>"
}
```

#### `IceCandidate`
```json
{
  "type": "ice_candidate",
  "session_id": "uuid",
  "candidate": "<ice_candidate>"
}
```

**Messages (Client → Server):**

#### `Hello`
```json
{
  "type": "hello"
}
```

#### `ConnectRequest`
```json
{
  "type": "connect_request",
  "machine_id": "uuid"
}
```

#### `SignalingOffer`
```json
{
  "type": "signaling_offer",
  "session_id": "uuid",
  "offer": "<sdp_offer>"
}
```

#### `IceCandidate`
```json
{
  "type": "ice_candidate",
  "session_id": "uuid",
  "candidate": "<ice_candidate>"
}
```

### Signaling State

Agent and client WebSocket sender channels are process-local. In production,
set `REDIS_URL` to persist ephemeral presence and routing metadata with TTLs:
`remotekvm:signaling:agent:{machine_id}` and
`remotekvm:signaling:session:{session_id}`. This lets operators see which
server instance owns an agent/session and supports load-balancer affinity, but
it does not move WebSocket delivery through Redis. Until a cross-instance
relay/pub-sub layer exists, `/client` traffic for a session must reach the same
server instance as the agent WebSocket.

---

## Error Responses

All errors follow this format:

```json
{
  "error": {
    "code": "MACHINE_NOT_FOUND",
    "message": "Machine with id 'uuid' not found"
  }
}
```

**Common Error Codes:**
- `UNAUTHORIZED` — Invalid or missing JWT
- `FORBIDDEN` — User lacks permission
- `MACHINE_NOT_FOUND` — Machine doesn't exist or user lacks access
- `MACHINE_OFFLINE` — Agent is not connected
- `RATE_LIMITED` — Too many requests
- `INVALID_INPUT` — Request body validation failed
- `INTERNAL_ERROR` — Server error

---

## Rate Limits

- Authentication endpoints: 10 requests per minute per IP
- API endpoints: 100 requests per minute per user
- WebSocket connections: 5 concurrent per user

---

## License
AGPL-3.0-or-later
