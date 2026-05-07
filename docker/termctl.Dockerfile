# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl \
    && mkdir -p /scratch-root/data
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked --target x86_64-unknown-linux-musl -p termctl --bin termctl \
    && strip /workspace/target/x86_64-unknown-linux-musl/release/termctl

FROM scratch
COPY --from=builder /workspace/target/x86_64-unknown-linux-musl/release/termctl /termctl
COPY --from=builder --chown=10001:10001 /scratch-root/data /data
ENV HOME=/data
WORKDIR /data
USER 10001:10001
ENTRYPOINT ["/termctl"]
CMD ["--help"]
