FROM rust:1.99-bookworm AS builder
WORKDIR /src
# Dependencies first, against stub sources: this layer is reused until a manifest or the lock changes.
COPY Cargo.toml Cargo.lock ./
COPY app/Cargo.toml app/Cargo.toml
COPY app/migration/Cargo.toml app/migration/Cargo.toml
COPY crates/core/Cargo.toml crates/core/Cargo.toml
RUN mkdir -p app/src/bin app/migration/src crates/core/src \
    && echo 'fn main() {}' > app/src/bin/main.rs \
    && touch app/src/lib.rs app/migration/src/lib.rs crates/core/src/lib.rs \
    && cargo build --release -p socialreins-brain --bin socialreins-brain
COPY . .
# Newer mtimes than the stub build, so cargo recompiles the workspace crates.
RUN find app crates -name '*.rs' -exec touch {} + \
    && cargo build --release -p socialreins-brain --bin socialreins-brain

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home brain
COPY --from=builder /src/target/release/socialreins-brain /usr/local/bin/socialreins-brain
USER brain
ENV PORT=8080
EXPOSE 8080
CMD ["socialreins-brain"]
