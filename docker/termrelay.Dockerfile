# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked -p termrelay --bin termrelay

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && adduser --system --uid 10001 --group --no-create-home termrelay
COPY --from=builder /workspace/target/release/termrelay /usr/local/bin/termrelay
RUN chmod 0755 /usr/local/bin/termrelay
USER termrelay
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/termrelay"]
CMD ["--listen", "0.0.0.0:8080"]

