FROM rust:1.85-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/mailmux /usr/local/bin/mailmux

RUN mkdir -p /etc/mailmux /var/lib/mailmux

VOLUME /var/lib/mailmux

ENTRYPOINT ["mailmux"]
CMD ["--config", "/etc/mailmux/config.toml"]
