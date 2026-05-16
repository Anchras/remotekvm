# Server API Specification

## Authentication

All API requests (except OAuth callbacks) require a Bearer JWT token in the `Authorization` header:

```
Authorization: Bearer <jwt_token>
```

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

### Authentication

#### `GET /auth/{provider}/start`

Initiates OAuth2 flow with the given provider.

**Providers:** `google`, `github`, `microsoft`

**Query Parameters:**
- `redirect_uri` (optional) — Where to redirect after auth

**Response:**
```json
{
  "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?..."
}
```

#### `GET /auth/{provider}/callback?code=...&state=...`

OAuth2 callback endpoint. Exchanges code for token, creates or updates user.

**Response:**
```json
{
  "token": "<jwt_token>",
  "user": {
    "id": "uuid",
    "email": "user@example.com",
    "name": "User Name",
    "avatar_url": "https://..."
  }
}
```

### User

#### `GET /api/me`

Returns the current authenticated user.

**Response:**
```json
{
  "id": "uuid",
  "email": "user@example.com",
  "name": "User Name",
  "avatar_url": "https://...",
  "teams": [
    {
      "id": "uuid",
      "name": "Acme Corp",
      "slug": "acme-corp",
      "role": "owner"
    }
  ]
}
```

### Machines

#### `GET /api/machines`

List all machines accessible to the current user (owned + team shared).

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
      "team": {
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

### Teams

#### `GET /api/teams`

List teams the user is a member of.

#### `POST /api/teams`

Create a new team.

**Request:**
```json
{
  "name": "Acme Corp"
}
```

#### `GET /api/teams/{id}`

Get team details.

#### `POST /api/teams/{id}/invite`

Invite a user to the team by email.

**Request:**
```json
{
  "email": "colleague@example.com",
  "role": "member"
}
```

#### `POST /api/teams/{id}/members/{user_id}/role`

Update a member's role.

**Request:**
```json
{
  "role": "admin"
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
