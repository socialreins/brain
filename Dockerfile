# syntax=docker/dockerfile:1
FROM rust:1.99-bookworm AS builder
WORKDIR /src
COPY . .
# Cache mounts keep the registry and target dir between builds; the binary is copied out of the mount.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p socialreins-brain --bin socialreins-brain \
    && cp target/release/socialreins-brain /socialreins-brain

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home brain
COPY --from=builder /socialreins-brain /usr/local/bin/socialreins-brain
USER brain
ENV PORT=8080
EXPOSE 8080
CMD ["socialreins-brain"]
