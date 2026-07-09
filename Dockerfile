FROM --platform=$BUILDPLATFORM rust:1.88-bookworm AS builder

ARG TARGETPLATFORM
ARG BUILDPLATFORM

RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        curl \
        pkg-config \
        ca-certificates \
        git \
        xz-utils \
        make \
        perl && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    rm -rf /var/lib/apt/lists/*

# zig + cargo-zigbuild: cross-compile Rust (incl. C deps) to musl targets
ENV ZIG_VERSION=0.13.0
RUN BUILD_ARCH=$(uname -m) && \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${BUILD_ARCH}-${ZIG_VERSION}.tar.xz" | tar -xJ -C /opt && \
    mv "/opt/zig-linux-${BUILD_ARCH}-${ZIG_VERSION}" /opt/zig && \
    ln -s /opt/zig/zig /usr/local/bin/zig
RUN cargo install cargo-zigbuild --version 0.20.0 --locked

# Map TARGETPLATFORM → Rust triple (musl, matches alpine runtime)
RUN case "$TARGETPLATFORM" in \
      "linux/amd64") echo "x86_64-unknown-linux-musl" > /tmp/rust_target ;; \
      "linux/arm64") echo "aarch64-unknown-linux-musl" > /tmp/rust_target ;; \
      *) echo "Unsupported TARGETPLATFORM: $TARGETPLATFORM" >&2 && exit 1 ;; \
    esac && \
    rustup target add "$(cat /tmp/rust_target)"

WORKDIR /build

# Frontend deps cached layer
COPY frontend/package.json frontend/package-lock.json frontend/
RUN cd frontend && npm ci

# Frontend build
COPY frontend frontend
RUN cd frontend && npx ng build --configuration=production

# Rust source
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
COPY tests tests

# Forgejo cargo registry auth (needed for private nzbdav-* crates)
ARG GIT_AUTH_TOKEN
ARG PLUGIN_PASSWORD
ARG RUSTNZB_BUILD_REF
ARG CI_COMMIT_SHA
ARG CI_COMMIT_TAG
ENV RUSTNZB_BUILD_REF=${RUSTNZB_BUILD_REF}
ENV CI_COMMIT_SHA=${CI_COMMIT_SHA}
ENV CI_COMMIT_TAG=${CI_COMMIT_TAG}
RUN TOKEN="${GIT_AUTH_TOKEN:-$PLUGIN_PASSWORD}" && \
    git config --global url."http://x-access-token:${TOKEN}@100.92.54.45:3002/".insteadOf "http://100.92.54.45:3002/" && \
    printf '[registries.forgejo]\nindex = "sparse+https://repo.indexarr.net/api/packages/indexarr/cargo/"\ncredential-provider = "cargo:token"\n\n[registry]\ndefault = "forgejo"\n' > $CARGO_HOME/config.toml && \
    printf '[registries.forgejo]\ntoken = "Bearer %s"\n' "$TOKEN" > $CARGO_HOME/credentials.toml
RUN sed -i '/^\[patch\./,/^$/d' Cargo.toml

ARG RELEASE_OPTIMIZED=false

RUN RUST_TARGET=$(cat /tmp/rust_target) && \
    if [ "$RELEASE_OPTIMIZED" = "true" ]; then \
      export CARGO_PROFILE_RELEASE_LTO=thin \
             CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
             CARGO_PROFILE_RELEASE_STRIP=symbols; \
    fi && \
    CARGO_INCREMENTAL=0 \
    cargo zigbuild --release --features webdav,vendored-openssl --target "$RUST_TARGET" && \
    cp "target/$RUST_TARGET/release/rustnzb" /build/rustnzb-out && \
    rm -rf target


FROM lscr.io/linuxserver/baseimage-alpine:3.21

RUN apk add --no-cache \
        ca-certificates \
        curl \
        7zip

COPY --from=builder /build/rustnzb-out /usr/local/bin/rustnzb

# s6 init: create directories and fix permissions
COPY root/ /

EXPOSE 9090

VOLUME ["/config", "/data", "/downloads"]
