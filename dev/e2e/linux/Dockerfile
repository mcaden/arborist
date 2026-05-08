# =============================================================================
# Arborist — Linux e2e Docker image
#
# Multi-stage build:
#   builder       → pre-built Tauri image; compiles the release AppImage
#   runtime-tools → same base; used for cargo test / vitest (source bind-mounted)
#   runtime-e2e   → thin runtime (no Rust): Xvfb, WebKitWebDriver, tauri-driver,
#                   Node + WebdriverIO, and the extracted AppImage
#
# Uses ivangabriele/tauri:debian-bookworm-22 as base for builder and
# runtime-tools. This community image (github.com/ivangabriele/docker-tauri)
# is based on the official rust:1-bookworm image and pre-installs:
#   Rust stable, Node 22 (LTS), webkit2gtk-4.1-dev, tauri-driver,
#   WebKitWebDriver, Xvfb, dbus-x11, and other Tauri build deps.
# Audited: Dockerfile is plain apt-get + cargo install, no custom scripts.
# Rebuilt daily via GitHub Actions CI.
# =============================================================================

# ---------------------------------------------------------------------------
# Stage 1: builder (compiles the release AppImage)
# ---------------------------------------------------------------------------
FROM ivangabriele/tauri:debian-bookworm-22 AS builder

# ---- Layer-cache trick: copy dependency manifests first, then source ----
WORKDIR /build

# Cargo workspace root manifests (Cargo.lock lives at repo root)
COPY Cargo.toml Cargo.lock ./
COPY src-tauri/Cargo.toml src-tauri/
# build.rs is needed for tauri-build to resolve
COPY src-tauri/build.rs src-tauri/

# npm manifests (for dependency caching)
COPY package.json package-lock.json ./

# Install npm deps (cached unless package*.json changes)
RUN npm ci --prefer-offline --no-audit

# Tauri config (needed before cargo can resolve the crate)
COPY src-tauri/tauri.conf.json src-tauri/tauri.conf.json
COPY src-tauri/capabilities/ src-tauri/capabilities/
COPY src-tauri/permissions/ src-tauri/permissions/
COPY src-tauri/icons/ src-tauri/icons/

# Now copy the rest of the source
COPY src/ src/
COPY src-tauri/src/ src-tauri/src/
COPY index.html tsconfig.json vite.config.ts vitest.config.ts postcss.config.js tailwind.config.js .prettierrc.json ./
# Instructions dir (needed at runtime for instruction-set tests)
COPY instructions/ instructions/

# Build the release bundle (produces AppImage + deb)
RUN npm run tauri:build

# arborist-test-child is a [[bin]] in the crate but tauri build only compiles the
# main binary. Build it explicitly (most deps are already cached from tauri:build).
RUN cargo build --release --bin arborist-test-child
RUN cp src-tauri/target/release/arborist-test-child /usr/local/bin/arborist-test-child

# Extract the AppImage so it can run without FUSE in unprivileged containers
RUN cd src-tauri/target/release/bundle/appimage \
    && chmod +x Arborist_*.AppImage \
    && ./Arborist_*.AppImage --appimage-extract \
    && mv squashfs-root /opt/arborist


# ---------------------------------------------------------------------------
# Stage 2: runtime-tools (for cargo test / vitest — source bind-mounted)
# ---------------------------------------------------------------------------
FROM ivangabriele/tauri:debian-bookworm-22 AS runtime-tools

COPY dev/e2e/linux/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

WORKDIR /src
ENTRYPOINT ["/entrypoint.sh"]


# ---------------------------------------------------------------------------
# Stage 3: runtime-e2e (thin — no Rust toolchain, only runtime libs)
# ---------------------------------------------------------------------------
FROM ubuntu:22.04 AS runtime-e2e

ENV DEBIAN_FRONTEND=noninteractive

# Runtime libs only (no -dev headers) + Xvfb + dbus + WebKitWebDriver
RUN apt-get update && apt-get install -y --no-install-recommends \
    libwebkit2gtk-4.1-0 \
    webkit2gtk-driver \
    libgtk-3-0 \
    libayatana-appindicator3-1 \
    librsvg2-2 \
    xvfb \
    dbus-x11 \
    xauth \
    x11-utils \
    git \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Node 22 LTS (needed for WebdriverIO — only stage that needs a fresh Node install
# since this stage does NOT extend the Tauri base image)
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Copy pre-built binaries from builder (no Rust toolchain needed here)
COPY --from=builder /root/.cargo/bin/tauri-driver /usr/local/bin/tauri-driver
COPY --from=builder /opt/arborist /opt/arborist
COPY --from=builder /usr/local/bin/arborist-test-child /usr/local/bin/arborist-test-child
COPY --from=builder /build/instructions /opt/arborist-instructions

# Install WebdriverIO deps into a fixed location
WORKDIR /e2e
COPY dev/e2e/linux/package.json dev/e2e/linux/package-lock.json* /e2e/
RUN npm ci --prefer-offline --no-audit || npm install

COPY dev/e2e/linux/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Environment for hermetic testing
ENV ARBORIST_CLI_OVERRIDE_CLAUDE=/usr/local/bin/arborist-test-child
ENV ARBORIST_CLI_OVERRIDE_COPILOT=/usr/local/bin/arborist-test-child
ENV DISPLAY=:99
ENV RUST_LOG=arborist_lib=info
ENV WEBKIT_DISABLE_COMPOSITING_MODE=1

ENTRYPOINT ["/entrypoint.sh"]
