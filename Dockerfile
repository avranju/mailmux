FROM rust:1.94-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY mailmux ./mailmux
COPY mailmux/migrations ./mailmux/migrations
COPY mailtx ./mailtx
COPY mailindex ./mailindex

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl jq \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/mailmux /usr/local/bin/mailmux
COPY --from=builder /build/target/release/mailtx /usr/local/bin/mailtx
COPY --from=builder /build/target/release/mailindex /usr/local/bin/mailindex
COPY --chmod=755 mailindex/contrib/mailmux-submit.sh /usr/local/bin/mailmux-submit

RUN mkdir -p /etc/mailmux /var/lib/mailmux

VOLUME /var/lib/mailmux

ENTRYPOINT ["mailmux"]
CMD ["--config", "/etc/mailmux/config.toml"]
