# Multi-stage build for iscsi-s3
FROM rust:1-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY vendor ./vendor
COPY examples ./examples
RUN cargo build --release --locked --bin iscsi-s3 --example smoke_client

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/iscsi-s3 /usr/local/bin/iscsi-s3
COPY --from=builder /app/target/release/examples/smoke_client /usr/local/bin/smoke_client
RUN useradd --system --create-home --uid 10001 iscsi
USER iscsi
EXPOSE 3260
ENTRYPOINT ["iscsi-s3"]
CMD ["--config", "/etc/iscsi-s3/config.toml", "--log", "info,iscsi_s3=debug,iscsi_target=debug"]
