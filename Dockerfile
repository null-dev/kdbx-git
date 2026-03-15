FROM rust:1.86-bookworm AS builder
WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /data
COPY --from=builder /app/target/release/kdbx-git /usr/local/bin/kdbx-git

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/kdbx-git"]
CMD ["config.toml"]
