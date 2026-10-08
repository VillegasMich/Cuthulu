# syntax=docker/dockerfile:1

# ── build: static musl binary ────────────────────────────────
# Runs on the build machine's platform and cross-compiles for the target
# one (linux/amd64 or linux/arm64), so multi-arch builds need no emulation.
# arm64 links with the toolchain's rust-lld; ring (rustls' crypto, via lettre
# and ureq) has C/asm that clang cross-compiles for it.
FROM --platform=$BUILDPLATFORM rust:1-alpine AS build
RUN apk add --no-cache musl-dev clang
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-musl ;; \
      arm64) triple=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && rustup target add "$triple" \
 && echo "$triple" >/triple
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
    CC_aarch64_unknown_linux_musl=clang
WORKDIR /src

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static
# The companion catalog, embedded for the env editor.
COPY deploy/companions ./deploy/companions

# Full git commit, shown next to the version in the UI and in /api/version
# (CI passes github.sha). Declared late so changing it only redoes this step.
ARG GIT_SHA=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    triple=$(cat /triple) \
 && CUTHULU_BUILD_SHA="$GIT_SHA" cargo build --release --locked --target "$triple" \
 && cp "target/$triple/release/cuthulu" /cuthulu
# `scratch` has no mkdir: prepare the data dir here, owned by the runtime user.
RUN mkdir -p /out/data

# ── runtime: just the binary ─────────────────────────────────
FROM scratch
COPY --from=build /cuthulu /cuthulu
COPY --from=build --chown=65532:65532 /out/ /

# Non-root. Access to the Docker socket is granted with --group-add.
USER 65532:65532
ENV CUTHULU_BIND=0.0.0.0:8686 \
    CUTHULU_DATA_DIR=/data \
    RUST_LOG=info
# Cuthulu's own state (todos.json). A named volume inherits the 65532 owner.
VOLUME /data
EXPOSE 8686
# Version (Cargo.toml's, e.g. 1.2.3) and commit of the build, for local
# builds (`--build-arg`). CI overrides them, and adds created, url, ..., with
# docker/metadata-action labels.
ARG VERSION=""
ARG GIT_SHA=""
LABEL org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${GIT_SHA}"
LABEL org.opencontainers.image.title="cuthulu" \
      org.opencontainers.image.description="The eye that never sleeps: dashboard for the Docker services on your machine" \
      org.opencontainers.image.source="https://github.com/VillegasMich/cuthulu" \
      org.opencontainers.image.licenses="MIT" \
      cuthulu.self="true"
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["/cuthulu", "healthcheck"]
ENTRYPOINT ["/cuthulu"]
