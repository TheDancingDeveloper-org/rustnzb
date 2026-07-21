# syntax=docker/dockerfile:1.7

ARG CROSS_IMAGE=repo.indexarr.net/indexarr/rustnzb-ci-cross@sha256:89f1f570acb0f8e6514ffcca39bd9f26305263d95274e4c170eedf867d113ad9

FROM --platform=$BUILDPLATFORM ${CROSS_IMAGE} AS builder

ARG TARGETPLATFORM
ARG RUSTNZB_BUILD_REF
ARG RELEASE_OPTIMIZED=false

WORKDIR /build

# Install and build the frontend only from its declared source and lockfile.
# Host node_modules, Angular output, and stale dist trees are excluded by
# .dockerignore and therefore cannot affect the runtime image.
COPY apps/rustnzb/frontend/package.json apps/rustnzb/frontend/package-lock.json apps/rustnzb/frontend/
RUN --mount=type=cache,target=/root/.npm,sharing=locked \
    npm --prefix apps/rustnzb/frontend ci --no-audit --no-fund
COPY apps/rustnzb/frontend apps/rustnzb/frontend
RUN npm --prefix apps/rustnzb/frontend run build -- --configuration=production \
    && test -s apps/rustnzb/frontend/dist/frontend/browser/index.html

# Cargo manifests and sources are copied only after the frontend dependency
# layer so ordinary application edits do not invalidate npm downloads.
COPY Cargo.toml Cargo.lock ./
COPY apps apps
COPY crates crates

# The Forgejo credential exists only in this BuildKit secret-mounted RUN. It
# is never a Docker ARG, image ENV value, Cargo file, or cache layer.
RUN --mount=type=secret,id=forgejo_token,required=true \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    set -eu; \
    case "$TARGETPLATFORM" in \
        linux/amd64) rust_target=x86_64-unknown-linux-musl ;; \
        linux/arm64) rust_target=aarch64-unknown-linux-musl ;; \
        *) printf 'unsupported target platform: %s\n' "$TARGETPLATFORM" >&2; exit 1 ;; \
    esac; \
    token=$(cat /run/secrets/forgejo_token); \
    if [ "$RELEASE_OPTIMIZED" = true ]; then \
        export CARGO_PROFILE_RELEASE_LTO=thin \
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
            CARGO_PROFILE_RELEASE_STRIP=symbols; \
    fi; \
    CARGO_REGISTRIES_FORGEJO_INDEX='sparse+https://repo.indexarr.net/api/packages/indexarr/cargo/' \
    CARGO_REGISTRIES_FORGEJO_CREDENTIAL_PROVIDER='cargo:token' \
    CARGO_REGISTRIES_FORGEJO_TOKEN="Bearer $token" \
    CARGO_TARGET_DIR=/build/target \
    RUSTNZB_BUILD_REF="$RUSTNZB_BUILD_REF" \
        cargo zigbuild --release --locked -p rustnzb \
            --features webdav,vendored-openssl --target "$rust_target"; \
    mkdir -p /out; \
    cp "/build/target/$rust_target/release/rustnzb" /out/rustnzb; \
    unset token CARGO_REGISTRIES_FORGEJO_TOKEN


FROM lscr.io/linuxserver/baseimage-alpine:3.23@sha256:46d690858431e262d574274bb2863e1fbaf8de61c6f7677150dd79c2cc65cdcf AS runtime

ARG RUSTNZB_BUILD_REF

RUN apk add --no-cache \
        7zip \
        ca-certificates \
        curl

COPY --from=builder /out/rustnzb /usr/local/bin/rustnzb
COPY apps/rustnzb/root/ /

LABEL org.opencontainers.image.title="rustnzb" \
      org.opencontainers.image.source="https://github.com/TheDancingDeveloper-org/rustnzb" \
      org.opencontainers.image.revision="$RUSTNZB_BUILD_REF"

EXPOSE 9090
VOLUME ["/config", "/data", "/downloads"]
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://127.0.0.1:9090/api/health || exit 1
