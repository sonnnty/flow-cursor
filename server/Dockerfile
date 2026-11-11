# FlowCursor hub image *without* agent builds (agents are not offered
# updates). Build from the repository root:
#   docker build -f server/Dockerfile -t flow-cursor-server:latest .
# build/build.sh builds the full image instead (build/Dockerfile.hub): the
# hub plus the Linux and Windows agents of the same build.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p flow-server \
 && install -D target/release/flow-server /out/flow-server \
 && install -d -o 65532 -g 65532 /out/data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/flow-server /usr/local/bin/flow-server
COPY --from=build --chown=65532:65532 /out/data /data
ENV FLOW_DATA=/data RUST_LOG=info
VOLUME ["/data"]
# QUIC for agents (UDP), dashboard (TCP), mDNS (UDP 5353, host network).
EXPOSE 7878/udp 7880/tcp
USER nonroot
ENTRYPOINT ["/usr/local/bin/flow-server"]
