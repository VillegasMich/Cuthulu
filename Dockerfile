# syntax=docker/dockerfile:1

# ── build: static musl binary ────────────────────────────────
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked \
 && cp target/release/cuthulu /cuthulu

# ── runtime: just the binary ─────────────────────────────────
FROM scratch
COPY --from=build /cuthulu /cuthulu

# Non-root. Access to the Docker socket is granted with --group-add.
USER 65532:65532
ENV CUTHULU_BIND=0.0.0.0:8686 \
    RUST_LOG=info
EXPOSE 8686
LABEL org.opencontainers.image.title="cuthulu" \
      org.opencontainers.image.description="The eye that never sleeps: dashboard for the Docker services on your machine" \
      org.opencontainers.image.source="https://github.com/VillegasMich/cuthulu" \
      org.opencontainers.image.licenses="MIT" \
      cuthulu.self="true"
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/cuthulu", "healthcheck"]
ENTRYPOINT ["/cuthulu"]
