#!/usr/bin/env bash
# =============================================================================
# Arborist macOS e2e — host-side runner script
#
# Automates SSH-based interactions with the macOS VM running in Docker.
# Handles: waiting for VM boot, syncing source, building, running tests,
# and opening interactive shells.
#
# Usage:
#   ./dev/e2e/macos/scripts/run.sh <mode> [extra-args...]
#
# Modes:
#   provision - One-time VM setup (install Rust, Node, Xcode CLI tools, etc.)
#   build     - Sync source + build the .app bundle inside the VM
#   e2e       - Sync source + build + run WebdriverIO e2e specs
#   shell     - Open interactive SSH session into the VM
#   status    - Check if the VM is reachable via SSH
#
# Environment:
#   MACOS_SSH_PORT - SSH port on localhost (default: 50922)
#   MACOS_USER    - VM username (default: arborist)
#   MACOS_PASS    - VM password (default: arborist)
# =============================================================================
set -euo pipefail

# Resolve script directory → repo root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
E2E_MACOS_DIR="${REPO_ROOT}/dev/e2e/macos"

MODE="${1:-status}"
shift || true

MACOS_SSH_PORT="${MACOS_SSH_PORT:-50922}"
MACOS_USER="${MACOS_USER:-arborist}"
MACOS_PASS="${MACOS_PASS:-arborist}"
MACOS_HOST="localhost"

SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -p ${MACOS_SSH_PORT}"
export SSHPASS="${MACOS_PASS}"

# ---- helpers ----------------------------------------------------------------

ssh_cmd() {
  sshpass -e ssh ${SSH_OPTS} "${MACOS_USER}@${MACOS_HOST}" "$@"
}

scp_to_vm() {
  sshpass -e scp ${SSH_OPTS} "$1" "${MACOS_USER}@${MACOS_HOST}:$2"
}

rsync_to_vm() {
  sshpass -e rsync -e "ssh ${SSH_OPTS}" "$@"
}

check_deps() {
  local missing=()
  for cmd in sshpass ssh scp rsync; do
    if ! command -v "$cmd" &>/dev/null; then
      missing+=("$cmd")
    fi
  done
  if [ ${#missing[@]} -gt 0 ]; then
    echo "[macos-e2e] ERROR: Missing required tools: ${missing[*]}" >&2
    echo "[macos-e2e] Install with:" >&2
    echo "  Ubuntu/Debian: sudo apt install sshpass openssh-client rsync" >&2
    echo "  macOS:         brew install sshpass rsync" >&2
    echo "  Windows/WSL2:  sudo apt install sshpass openssh-client rsync" >&2
    exit 1
  fi
}

wait_for_vm() {
  echo "[macos-e2e] Waiting for macOS VM SSH at ${MACOS_HOST}:${MACOS_SSH_PORT}..."
  local tries=0
  local max_tries=180  # 3 minutes

  while ! sshpass -e ssh ${SSH_OPTS} -o ConnectTimeout=2 "${MACOS_USER}@${MACOS_HOST}" "echo ok" >/dev/null 2>&1; do
    tries=$((tries + 1))
    if [ $tries -ge $max_tries ]; then
      echo "[macos-e2e] ERROR: VM not reachable after ${max_tries}s" >&2
      echo "[macos-e2e] Is the VM running? Try: docker compose -f dev/e2e/macos/docker-compose.yml up -d" >&2
      echo "[macos-e2e] Is the VM provisioned? See dev/e2e/macos/README.md" >&2
      exit 1
    fi
    if [ $((tries % 15)) -eq 0 ]; then
      echo "[macos-e2e]   ...still waiting (${tries}s elapsed)"
    fi
    sleep 1
  done
  echo "[macos-e2e] VM is reachable"
}

sync_source() {
  echo "[macos-e2e] Syncing source to VM (~/${PROJECT_DIR:-arborist})..."
  ssh_cmd "mkdir -p ~/arborist"
  rsync_to_vm -az --delete \
    --exclude='target/' \
    --exclude='node_modules/' \
    --exclude='.git/objects/' \
    --exclude='*.AppImage' \
    --exclude='.arborist/.worktrees/' \
    "${REPO_ROOT}/" "${MACOS_USER}@${MACOS_HOST}:~/arborist/"
  echo "[macos-e2e] Source synced to ~/arborist"
}

# ---- modes ------------------------------------------------------------------

run_provision() {
  wait_for_vm
  echo "[macos-e2e] Running provisioning script inside VM..."
  ssh_cmd 'bash -s' < "${E2E_MACOS_DIR}/scripts/provision-vm.sh"
  echo ""
  echo "[macos-e2e] Provisioning complete. Install tauri-driver:"
  ssh_cmd 'source "$HOME/.cargo/env" && cargo install tauri-cli 2>/dev/null || echo "(tauri-cli may already be installed)"'
  echo "[macos-e2e] Done. VM is ready for builds and tests."
}

run_build() {
  wait_for_vm
  sync_source

  echo "[macos-e2e] Building Arborist .app bundle..."
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && pnpm install --frozen-lockfile'
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && pnpm run build'
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && cargo build --release --features test-helpers --bin arborist-test-child'
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && pnpm tauri build --bundles app'

  echo ""
  echo "[macos-e2e] Build complete!"
  ssh_cmd 'ls -la ~/arborist/target/release/bundle/macos/'
}

run_e2e() {
  wait_for_vm
  sync_source

  # Build the app
  echo "[macos-e2e] Building app for e2e testing..."
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && pnpm install --frozen-lockfile'
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && pnpm run build'
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && cargo build --release --features test-helpers --bin arborist-test-child'
  ssh_cmd 'source "$HOME/.cargo/env" && cd ~/arborist && pnpm tauri build --bundles app'

  # Set up test workspace
  echo "[macos-e2e] Setting up test workspace..."
  ssh_cmd 'if [ ! -d /tmp/arborist-test-workspace/.git ]; then
    rm -rf /tmp/arborist-test-workspace
    mkdir -p /tmp/arborist-test-workspace
    git -C /tmp/arborist-test-workspace init -q -b main
    git -C /tmp/arborist-test-workspace config user.email "e2e@arborist.local"
    git -C /tmp/arborist-test-workspace config user.name "Arborist E2E"
    echo "# Arborist e2e test workspace" > /tmp/arborist-test-workspace/README.md
    git -C /tmp/arborist-test-workspace add README.md
    git -C /tmp/arborist-test-workspace commit -q -m "initial commit"
  fi'

  # Copy and install e2e test deps
  echo "[macos-e2e] Setting up WebdriverIO test environment..."
  ssh_cmd "mkdir -p ~/e2e-specs"
  rsync_to_vm -az --delete \
    "${REPO_ROOT}/dev/e2e/linux/specs/" "${MACOS_USER}@${MACOS_HOST}:~/e2e-specs/"
  scp_to_vm "${E2E_MACOS_DIR}/scripts/wdio.macos.conf.ts" "~/e2e-specs/wdio.conf.ts"

  ssh_cmd 'cd ~/e2e-specs && cat > package.json << '\''EOF'\''
{
  "name": "arborist-e2e-macos",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "devDependencies": {
    "@wdio/cli": "^9.19.0",
    "@wdio/local-runner": "^9.19.0",
    "@wdio/mocha-framework": "^9.19.0",
    "@wdio/spec-reporter": "^9.19.0",
    "ts-node": "^10.9.2",
    "typescript": "^5.7.2"
  }
}
EOF'
  ssh_cmd 'cd ~/e2e-specs && npm install 2>/dev/null'

  # Run the e2e tests
  echo "[macos-e2e] Running WebdriverIO specs..."
  set +e
  ssh_cmd "cd ~/e2e-specs && npx wdio run wdio.conf.ts $*"
  local rc=$?
  set -e

  echo "[macos-e2e] WebdriverIO exited with code $rc"
  exit $rc
}

run_shell() {
  wait_for_vm
  echo "[macos-e2e] Opening interactive SSH session..."
  echo "[macos-e2e] Source at: ~/arborist (after sync)"
  echo ""
  exec sshpass -e ssh ${SSH_OPTS} -t "${MACOS_USER}@${MACOS_HOST}"
}

run_status() {
  check_deps
  echo "[macos-e2e] Checking VM status..."
  if sshpass -e ssh ${SSH_OPTS} -o ConnectTimeout=3 "${MACOS_USER}@${MACOS_HOST}" "echo ok" >/dev/null 2>&1; then
    echo "[macos-e2e] ✓ VM is running and SSH is reachable"
    ssh_cmd "sw_vers 2>/dev/null || echo '(sw_vers not available)'"
  else
    echo "[macos-e2e] ✗ VM is not reachable on ${MACOS_HOST}:${MACOS_SSH_PORT}"
    echo "[macos-e2e]   Start with: docker compose -f dev/e2e/macos/docker-compose.yml up -d"
  fi
}

# ---- dispatch ---------------------------------------------------------------

check_deps

case "$MODE" in
  provision) run_provision "$@" ;;
  build)     run_build "$@" ;;
  e2e)       run_e2e "$@" ;;
  shell)     run_shell "$@" ;;
  status)    run_status "$@" ;;
  *)
    echo "[macos-e2e] Unknown mode: $MODE" >&2
    echo "Usage: run.sh {provision|build|e2e|shell|status}" >&2
    exit 1
    ;;
esac
