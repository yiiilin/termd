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
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p termd --bin termd

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && adduser --system --uid 10001 --group --no-create-home termd
COPY --from=builder /workspace/target/release/termd /usr/local/bin/termd
RUN chmod 0755 /usr/local/bin/termd
USER termd
EXPOSE 8765
ENTRYPOINT ["/usr/local/bin/termd"]
CMD ["--listen", "0.0.0.0:8765"]
