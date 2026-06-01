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

# clang + lld for the faster linker configured by .cargo/config.toml.
# Without these the cargo build would fall back to the system linker (since
# the config specifies `linker = "clang"` + `-fuse-ld=lld`), but cargo
# refuses to spawn a missing linker.
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    lld \
    && rm -rf /var/lib/apt/lists/*

# ---- Layer-cache trick: copy dependency manifests first, then source ----
WORKDIR /build

# Workspace `.cargo/config.toml` (pins the linker + opt-in sccache wrapper).
# Copied before any `cargo` invocation so all subsequent compiles see it.
COPY .cargo/ ./.cargo/

# Cargo workspace root manifests (Cargo.lock lives at repo root)
COPY Cargo.toml Cargo.lock ./
COPY src-tauri/Cargo.toml src-tauri/
# build.rs is needed for tauri-build to resolve
COPY src-tauri/build.rs src-tauri/
# arborist-types workspace crate (pure wire-contract types). Copied alongside
# the manifests so the workspace `cargo metadata` resolves and the
# layer-cache step below can prime dependency builds before source changes.
COPY crates/ crates/

# pnpm + workspace manifests (for dependency caching)
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
RUN corepack enable && corepack prepare pnpm@10.33.0 --activate

# Install deps (cached unless pnpm-lock.yaml changes)
RUN pnpm install --frozen-lockfile

# Tauri config (needed before cargo can resolve the crate)
COPY src-tauri/tauri.conf.json src-tauri/tauri.conf.json
COPY src-tauri/capabilities/ src-tauri/capabilities/
COPY src-tauri/permissions/ src-tauri/permissions/
COPY src-tauri/icons/ src-tauri/icons/
# AppImage / deb / rpm bundles reference `metainfo/io.github.mcaden.arborist.metainfo.xml`
# in tauri.conf.json under bundle.linux.{appimage,deb,rpm}.files — the bundler
# resolves the path relative to src-tauri/, so the directory must be present in
# the build context before `pnpm tauri build`.
COPY src-tauri/metainfo/ src-tauri/metainfo/

# Now copy the rest of the source
COPY src/ src/
COPY src-tauri/src/ src-tauri/src/
COPY index.html tsconfig.json vite.config.ts vitest.config.ts postcss.config.js tailwind.config.js .prettierrc.json ./

# Build the frontend (needed before any cargo build — tauri::generate_context!()
# requires ../dist to exist)
RUN pnpm run build

# Pre-build the test-child binary. The runtime-e2e stage copies it to
# /usr/local/bin/arborist-test-child (line below) so the extracted AppImage
# can be exercised against this hermetic CLI override instead of real
# claude / copilot. test-locker is included so any future cargo-test stage
# can use it without a second build.
RUN cargo build --release --features test-helpers --bin arborist-test-child --bin arborist-test-locker

# Build the release bundle — AppImage only (skip deb to avoid bundler issues)
RUN pnpm tauri build --bundles appimage

# Copy test-child to a stable path for the runtime-e2e stage. Cargo
# workspace target dir is at the repo root (`/build/target/`), not under
# `src-tauri/`, because the workspace root is `/build/Cargo.toml`.
RUN cp target/release/arborist-test-child /usr/local/bin/arborist-test-child

# Extract the AppImage so it can run without FUSE in unprivileged containers
RUN cd target/release/bundle/appimage \
    && chmod +x Arborist_*.AppImage \
    && ./Arborist_*.AppImage --appimage-extract \
    && mv squashfs-root /opt/arborist


# ---------------------------------------------------------------------------
# Stage 2: runtime-tools (for cargo test / vitest — source bind-mounted)
# ---------------------------------------------------------------------------
FROM ivangabriele/tauri:debian-bookworm-22 AS runtime-tools

# clang + lld for the faster linker configured by .cargo/config.toml. Same
# rationale as the builder stage. The runtime-tools stage bind-mounts source
# from the host at /src, so cargo discovers `/src/.cargo/config.toml` and
# needs these tools at compile time.
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang \
    lld \
    && rm -rf /var/lib/apt/lists/*

COPY dev/e2e/linux/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

WORKDIR /src
ENTRYPOINT ["/entrypoint.sh"]


# ---------------------------------------------------------------------------
# Stage 3: runtime-e2e (thin — no Rust toolchain, only runtime libs)
#
# Base must match the builder's distro family. The builder uses
# `ivangabriele/tauri:debian-bookworm-22` (= rust:bookworm), so its bundled
# AppImage links against bookworm's libpango / libharfbuzz versions. Running
# that AppImage on Ubuntu 22.04 (older harfbuzz) fails with:
#   "symbol lookup error: libpango-1.0.so.0: undefined symbol:
#    hb_ot_layout_get_horizontal_baseline_tag_for_script"
# Using debian:bookworm-slim keeps the C-library universe consistent end to end.
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime-e2e

ENV DEBIAN_FRONTEND=noninteractive

# Runtime libs only (no -dev headers) + Xvfb + dbus + WebKitWebDriver
#
# Audio note: libasound2 is a hard runtime dep of WebKit2GTK 4.1 (audio
# backend). Without it the AppImage exits immediately at startup with
# "error while loading shared libraries: libasound.so.2". apt-get's
# --no-install-recommends omits it, so it must be listed explicitly.
RUN apt-get update && apt-get install -y --no-install-recommends \
    libwebkit2gtk-4.1-0 \
    webkit2gtk-driver \
    libgtk-3-0 \
    libayatana-appindicator3-1 \
    librsvg2-2 \
    libasound2 \
    xvfb \
    dbus-x11 \
    xauth \
    x11-utils \
    git \
    curl \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Node 24 LTS (needed for WebdriverIO — only stage that needs a fresh Node install
# since this stage does NOT extend the Tauri base image)
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/*

# Copy pre-built binaries from builder (no Rust toolchain needed here).
# tauri-driver is `cargo install`-ed in the upstream image. The official
# rust:bookworm base sets CARGO_HOME=/usr/local/cargo, so cargo-installed
# binaries live under /usr/local/cargo/bin/ — *not* /root/.cargo/bin/.
COPY --from=builder /usr/local/cargo/bin/tauri-driver /usr/local/bin/tauri-driver
COPY --from=builder /opt/arborist /opt/arborist
COPY --from=builder /usr/local/bin/arborist-test-child /usr/local/bin/arborist-test-child

# Install WebdriverIO deps via pnpm (matches the rest of the repo — root
# package.json pins `packageManager: pnpm@10.33.0` and the canonical lockfile
# format here is pnpm-lock.yaml). Corepack ships with Node and provides pnpm;
# we activate the pinned version so installs are reproducible.
#
# `--ignore-workspace` is defensive — there is no pnpm-workspace.yaml inside
# this image, but the flag makes the install bulletproof against any future
# workspace file that might land at / or /e2e.
#
# COREPACK_HOME pins Corepack's pnpm cache to an image-baked path. Without it,
# Corepack defaults to `$HOME/.cache/node/corepack`, and entrypoint.sh sets
# HOME=/tmp/arborist-home for hermetic test runs — so at runtime Corepack would
# look at the (empty) ephemeral HOME, miss the cache, and re-download pnpm on
# every `docker compose run --rm e2e`. Pinning to /opt/corepack keeps the
# activated pnpm available regardless of HOME.
ENV COREPACK_HOME=/opt/corepack
WORKDIR /e2e
COPY dev/e2e/linux/package.json dev/e2e/linux/pnpm-lock.yaml /e2e/
RUN corepack enable \
    && corepack prepare pnpm@10.33.0 --activate \
    && pnpm install --frozen-lockfile --ignore-workspace

COPY dev/e2e/linux/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# Environment for hermetic testing
ENV DISPLAY=:99
ENV RUST_LOG=arborist_lib=info
ENV WEBKIT_DISABLE_COMPOSITING_MODE=1

ENTRYPOINT ["/entrypoint.sh"]
