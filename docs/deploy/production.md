# RemoteKVM Production Deployment

The production path for the server is Fly.io with managed Postgres and managed
Redis. Fly terminates TLS at the edge, preserves WebSocket upgrades, performs
health checks against `/health`, and streams application logs without adding a
separate reverse proxy tier.

## Topology

- Fly app: `remotekvm-server`, configured by `fly.toml`.
- Runtime image: `apps/server/Dockerfile`.
- TLS/reverse proxy: Fly HTTP service with `force_https = true`.
- Server port: `PORT=8080`.
- Health check: `GET /health` every 30 seconds.
- Database: Fly Postgres, exposed to the app as `DATABASE_URL`.
- Redis: Upstash Redis or a Fly Redis-compatible service, exposed as `REDIS_URL`.
- Logs: `tracing_subscriber` stdout/stderr, collected by `fly logs`.

The `/agent` and `/client` WebSocket routes are long-lived. Keep
`auto_stop_machines = false` and at least one machine running so connected
agents are not evicted during idle periods. Horizontal scale is possible, but
the current relay keeps WebSocket sender channels in process. Use one machine or
sticky routing until cross-instance relay/pub-sub is added.

## First Deploy

```sh
fly launch --no-deploy --copy-config
fly postgres create --name remotekvm-postgres --region iad
fly postgres attach --app remotekvm-server remotekvm-postgres
fly secrets set \
  JWT_SECRET="$(openssl rand -base64 48)" \
  WORKOS_API_KEY="sk_live_..." \
  WORKOS_CLIENT_ID="client_..." \
  STRIPE_SECRET_KEY="sk_live_..." \
  STRIPE_WEBHOOK_SECRET="whsec_..." \
  STRIPE_PRICE_ID="price_..." \
  REDIS_URL="rediss://default:...@...:6379" \
  SIGNALING_INSTANCE_ID="fly-primary"
fly deploy
```

Set `PUBLIC_BASE_URL` in `fly.toml` to the final domain before configuring
WorkOS. If using a custom domain:

```sh
fly certs add api.remotekvm.example
fly secrets set PUBLIC_BASE_URL="https://api.remotekvm.example"
```

WorkOS must redirect to:

```text
https://<PUBLIC_BASE_URL_HOST>/auth/workos/callback
```

Stripe webhooks must target:

```text
https://<PUBLIC_BASE_URL_HOST>/webhooks/stripe
```

## Migrations

The server runs embedded sqlx migrations at startup. For manual verification or
emergency runs:

```sh
fly ssh console -C "remotekvm-server --help"
fly proxy 15432:5432 -a <postgres-app-name>
DATABASE_URL=postgres://...@127.0.0.1:15432/... sqlx migrate run --source apps/server/migrations
```

The normal deploy path is `fly deploy`; startup applies migrations before the
HTTP listener is bound.

## Environment And Secrets

Use `apps/server/.env.example` as the complete local template. In production,
store secrets with `fly secrets set`; do not bake them into images.

Required production values:

- `DATABASE_URL`
- `JWT_SECRET`, at least 32 bytes
- `WORKOS_API_KEY`
- `WORKOS_CLIENT_ID`
- `PUBLIC_BASE_URL`
- `STRIPE_SECRET_KEY`
- `STRIPE_WEBHOOK_SECRET`
- `STRIPE_PRICE_ID`

Recommended production values:

- `REDIS_URL`
- `SIGNALING_INSTANCE_ID`
- `SIGNALING_TTL_SECONDS=90`
- `RUST_LOG=info,remotekvm_server=info,tower_http=info`

## Monitoring And Logging

Baseline checks:

```sh
fly status
fly checks list
fly logs
curl -fsS https://remotekvm-server.fly.dev/health
```

Alert on:

- `/health` failures or repeated restarts.
- Postgres connection saturation.
- 5xx spikes on REST routes.
- WebSocket disconnect churn on `/agent`.
- Redis connection errors when multi-instance metadata is enabled.

Application logs are structured enough for Fly log drains. Add a Fly log shipper
or drain to the team logging system when retention beyond Fly's live logs is
needed.

## Runbook

Deploy:

```sh
cargo test -p remotekvm-server --lib
cargo test -p remotekvm-server --test integration --no-run
fly deploy
fly checks list
```

Rollback:

```sh
fly releases
fly deploy --image registry.fly.io/remotekvm-server:<previous-version>
```

Restart the app:

```sh
fly apps restart remotekvm-server
```

Rotate secrets:

```sh
fly secrets set JWT_SECRET="$(openssl rand -base64 48)"
fly deploy
```

For `JWT_SECRET`, expect existing client sessions to be invalidated after the
new machines start.

Investigate a failed health check:

```sh
fly logs
fly ssh console
curl -v http://127.0.0.1:8080/health
```

If the server fails before binding, check missing secrets and database
connectivity first. `Config::from_env` rejects empty WorkOS credentials, short
JWT secrets, and zero `SIGNALING_TTL_SECONDS`.
