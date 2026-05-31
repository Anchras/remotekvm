FROM rust:1.86-bookworm AS builder

WORKDIR /app
COPY . .

RUN cargo build --release -p remotekvm-server

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/remotekvm-server /usr/local/bin/remotekvm-server

ENV PORT=8080
EXPOSE 8080

CMD ["remotekvm-server"]
