FROM rust:1.94-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY mailmux ./mailmux
COPY mailmux/migrations ./mailmux/migrations
COPY mailtx ./mailtx

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/mailmux /usr/local/bin/mailmux
COPY --from=builder /build/target/release/mailtx /usr/local/bin/mailtx

RUN mkdir -p /etc/mailmux /var/lib/mailmux

VOLUME /var/lib/mailmux

ENTRYPOINT ["mailmux"]
CMD ["--config", "/etc/mailmux/config.toml"]
