# vm-gateway for Linux (EC2 or any VM). Build from the repository root:
#   docker build -f deploy/gateway/Dockerfile -t vm-gateway .
# The build runs inside Docker; it does not touch the host's target/.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# Only the gateway and its two library crates are compiled.
RUN cargo build --release --locked -p vm-gateway \
 && cp target/release/vm-gateway /vm-gateway

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /vm-gateway /usr/local/bin/vm-gateway
# /data holds gateway.toml, the tunnel key and ACME state (0600 files).
VOLUME ["/data"]
EXPOSE 80/tcp 443/tcp 443/udp 7443/udp
ENTRYPOINT ["/usr/local/bin/vm-gateway", "--config", "/data/gateway.toml"]
