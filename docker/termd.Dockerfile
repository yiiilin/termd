# syntax=docker/dockerfile:1.7

FROM node:22-bookworm AS web-builder
WORKDIR /workspace/termui/frontend
COPY termui/frontend/package*.json ./
RUN npm ci
COPY termui/frontend/ ./
RUN npm run build

FROM rust:1-bookworm AS builder
WORKDIR /workspace
COPY . .
COPY --from=web-builder /workspace/termui/frontend/dist /workspace/termui/frontend/dist
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl \
    && mkdir -p /scratch-root/data
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked --target x86_64-unknown-linux-musl -p termd --bin termd \
    && strip /workspace/target/x86_64-unknown-linux-musl/release/termd

FROM scratch
COPY --from=builder /workspace/target/x86_64-unknown-linux-musl/release/termd /termd
COPY --from=builder --chown=10001:10001 /scratch-root/data /data
ENV HOME=/data
WORKDIR /data
USER 10001:10001
EXPOSE 8765
ENTRYPOINT ["/termd"]
CMD ["--listen", "0.0.0.0:8765"]
