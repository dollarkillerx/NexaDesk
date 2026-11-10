# nexa-signal: signaling, STUN and relay in one process (docs/DEPLOY.md).
#
#   docker build -f deploy/nexa-signal.Dockerfile -t nexa-signal .
#
# Build from the repository root. Run with host networking (deploy/docker-compose.yml):
# STUN must see clients' real addresses, and the relay uses a UDP port per server.

FROM rust:1.99-trixie AS build
WORKDIR /src
# Only what the build reads; the workspace manifest lists every crate.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p nexa-signal \
 && install -m 0755 target/release/nexa-signal /nexa-signal

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends libcap2-bin ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /nonexistent --shell /usr/sbin/nologin nexa-signal
COPY --from=build /nexa-signal /usr/local/bin/nexa-signal
# Port 443 without running as root.
RUN setcap cap_net_bind_service=+ep /usr/local/bin/nexa-signal \
 && apt-get purge -y libcap2-bin && apt-get autoremove -y
USER 10001:10001
# Signaling (TCP, wss:// and the /relay fallback), STUN (UDP), relay (UDP).
EXPOSE 443/tcp 3478/udp 3479/udp 3480/udp
ENTRYPOINT ["/usr/local/bin/nexa-signal"]
CMD ["--config", "/etc/nexa-signal/nexa-signal.toml"]
