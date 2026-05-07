# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p termctl --bin termctl

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && adduser --system --uid 10001 --group --no-create-home termctl
COPY --from=builder /workspace/target/release/termctl /usr/local/bin/termctl
RUN chmod 0755 /usr/local/bin/termctl
USER termctl
ENTRYPOINT ["/usr/local/bin/termctl"]
CMD ["--help"]

