#!/usr/bin/env bash
# =============================================================================
# Arborist Linux e2e - unified entrypoint
#
# Usage:  entrypoint.sh <mode> [extra-args...]
#   e2e     - start Xvfb + tauri-driver, run WebdriverIO specs
#   rust    - cargo test --workspace --features test-helpers
#   vitest  - pnpm install && pnpm test --run
#   shell   - start Xvfb + tauri-driver, drop into bash
# =============================================================================
set -euo pipefail

MODE="${1:-e2e}"
shift || true

# ---- helpers ----------------------------------------------------------------

start_xvfb() {
  echo "[entrypoint] Starting Xvfb on :99 (1280x1024x24)..."
  Xvfb :99 -screen 0 1280x1024x24 -nolisten tcp &
  XVFB_PID=$!
  export DISPLAY=:99

  # Wait until the X server is ready
  local tries=0
  while ! xdpyinfo -display :99 >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ $tries -ge 30 ]; then
      echo "[entrypoint] ERROR: Xvfb failed to start after 30 attempts" >&2
      exit 1
    fi
    sleep 0.2
  done
  echo "[entrypoint] Xvfb ready (PID $XVFB_PID)"
}

start_dbus() {
  echo "[entrypoint] Starting dbus session..."
  eval "$(dbus-launch --sh-syntax)" || true
  export DBUS_SESSION_BUS_ADDRESS
  echo "[entrypoint] D-Bus ready (${DBUS_SESSION_BUS_ADDRESS:-none})"
}

setup_home() {
  # Use a fresh ephemeral HOME so each run starts clean (no leftover config)
  export HOME="${ARBORIST_E2E_HOME:-/tmp/arborist-home}"
  mkdir -p "$HOME"
  echo "[entrypoint] HOME=$HOME"
}

# Pre-create a temp git repo so the e2e arborist boot has a workspace to bind
# without invoking the native folder picker (which would block forever in a
# headless container — boot resolution is `--workspace CLI arg → hint file →
# legacy config → native picker`, and only the first arm is non-interactive).
# wdio.conf.ts passes this path via tauri:options.args = ["--workspace", …].
setup_test_workspace() {
  export ARBORIST_TEST_WORKSPACE="${ARBORIST_TEST_WORKSPACE:-/tmp/arborist-test-workspace}"
  if [ ! -d "$ARBORIST_TEST_WORKSPACE/.git" ]; then
    rm -rf "$ARBORIST_TEST_WORKSPACE"
    mkdir -p "$ARBORIST_TEST_WORKSPACE"
    git -C "$ARBORIST_TEST_WORKSPACE" init -q -b main
    git -C "$ARBORIST_TEST_WORKSPACE" config user.email "e2e@arborist.local"
    git -C "$ARBORIST_TEST_WORKSPACE" config user.name "Arborist E2E"
    echo "# Arborist e2e test workspace" > "$ARBORIST_TEST_WORKSPACE/README.md"
    git -C "$ARBORIST_TEST_WORKSPACE" add README.md
    git -C "$ARBORIST_TEST_WORKSPACE" commit -q -m "initial commit"
  fi
  echo "[entrypoint] ARBORIST_TEST_WORKSPACE=$ARBORIST_TEST_WORKSPACE"
}

# ---- modes ------------------------------------------------------------------

run_e2e() {
  setup_home
  setup_test_workspace
  start_xvfb
  start_dbus

  echo "[entrypoint] Running WebdriverIO specs..."
  cd /e2e

  # Specs are bind-mounted to /specs at runtime; copy wdio config if present
  if [ -f /specs/wdio.conf.ts ]; then
    echo "[entrypoint] Using bind-mounted wdio.conf.ts from /specs/"
  fi

  # Run the specs
  pnpm exec wdio run /specs/wdio.conf.ts "$@"
  local rc=$?

  echo "[entrypoint] WebdriverIO exited with code $rc"
  exit $rc
}

run_rust() {
  # `--features test-helpers` matches the local quality gate and is required to
  # build / run the PTY and workspace-lock integration tests, which depend on
  # the `arborist-test-child` and `arborist-test-locker` binaries (both gated
  # behind `required-features = ["test-helpers"]` in src-tauri/Cargo.toml).
  echo "[entrypoint] Running cargo test --workspace --features test-helpers..."
  cd /src
  cargo test --workspace --features test-helpers --no-fail-fast "$@"
}

run_vitest() {
  echo "[entrypoint] Running pnpm install + vitest..."
  cd /src
  corepack enable && corepack prepare pnpm@10.33.0 --activate
  pnpm install --frozen-lockfile
  pnpm test -- --run "$@"
}

run_shell() {
  setup_home
  setup_test_workspace
  start_xvfb
  start_dbus

  echo "[entrypoint] Dropping into bash. Xvfb is running on :99."
  echo "[entrypoint] AppImage extracted at /opt/arborist/"
  echo "[entrypoint] tauri-driver is at /usr/local/bin/tauri-driver"
  echo "[entrypoint] arborist-test-child is at /usr/local/bin/arborist-test-child"
  echo "[entrypoint] Test workspace: $ARBORIST_TEST_WORKSPACE"
  echo ""
  exec bash "$@"
}

# ---- dispatch ---------------------------------------------------------------

case "$MODE" in
  e2e)     run_e2e "$@" ;;
  rust)    run_rust "$@" ;;
  vitest)  run_vitest "$@" ;;
  shell)   run_shell "$@" ;;
  *)
    echo "[entrypoint] Unknown mode: $MODE" >&2
    echo "Usage: entrypoint.sh {e2e|rust|vitest|shell}" >&2
    exit 1
    ;;
esac
