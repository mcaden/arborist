#!/usr/bin/env bash
# =============================================================================
# Arborist — macOS VM one-time provisioning script
#
# Run this INSIDE the macOS VM after initial OS installation.
# It installs all build dependencies needed for Arborist e2e testing.
#
# Usage (from the orchestrator shell or direct SSH):
#   ssh arborist@<vm-ip> 'bash -s' < scripts/provision-vm.sh
#
# Or from within the VM:
#   bash /Volumes/shared/dev/e2e/macos/scripts/provision-vm.sh
# =============================================================================
set -euo pipefail

echo "=== Arborist macOS VM Provisioning ==="
echo ""

# ---- 1. Xcode Command Line Tools -------------------------------------------
echo "[1/7] Installing Xcode Command Line Tools..."
if ! xcode-select -p &>/dev/null; then
  # Trigger the install prompt. In a headless environment, this may need to be
  # done interactively via the web viewer (port 8006).
  xcode-select --install 2>/dev/null || true
  echo "  → Please complete Xcode CLI Tools installation via the GUI if prompted."
  echo "  → Then re-run this script."
  echo "  → (Connect to http://localhost:8006 to see the macOS desktop)"
  exit 1
else
  echo "  ✓ Xcode CLI Tools already installed"
fi

# ---- 2. Homebrew ------------------------------------------------------------
echo "[2/7] Installing Homebrew..."
if ! command -v brew &>/dev/null; then
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
  # Add brew to PATH for Apple Silicon
  if [ -f /opt/homebrew/bin/brew ]; then
    eval "$(/opt/homebrew/bin/brew shellenv)"
    echo 'eval "$(/opt/homebrew/bin/brew shellenv)"' >> ~/.zprofile
  fi
else
  echo "  ✓ Homebrew already installed"
fi

# ---- 3. Rust ----------------------------------------------------------------
echo "[3/7] Installing Rust..."
if ! command -v rustc &>/dev/null; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source "$HOME/.cargo/env"
else
  echo "  ✓ Rust already installed ($(rustc --version))"
fi

# ---- 4. Node.js + pnpm -----------------------------------------------------
echo "[4/7] Installing Node.js and pnpm..."
if ! command -v node &>/dev/null; then
  brew install node@22
  brew link node@22 --force --overwrite
else
  echo "  ✓ Node.js already installed ($(node --version))"
fi

corepack enable 2>/dev/null || true
corepack prepare pnpm@10.33.0 --activate 2>/dev/null || true
echo "  ✓ pnpm ready ($(pnpm --version 2>/dev/null || echo 'installing...'))"

# ---- 5. Build dependencies --------------------------------------------------
echo "[5/7] Installing build dependencies..."
# LLD for faster linking (matches .cargo/config.toml)
brew install llvm 2>/dev/null || true
# Git (usually pre-installed but ensure latest)
brew install git 2>/dev/null || true
echo "  ✓ Build dependencies installed"

# Ensure llvm/lld is on PATH
LLVM_PREFIX="$(brew --prefix llvm 2>/dev/null || echo /opt/homebrew/opt/llvm)"
if [ -d "${LLVM_PREFIX}/bin" ]; then
  export PATH="${LLVM_PREFIX}/bin:$PATH"
  if ! grep -q 'llvm/bin' ~/.zprofile 2>/dev/null; then
    echo "export PATH=\"${LLVM_PREFIX}/bin:\$PATH\"" >> ~/.zprofile
  fi
fi

# ---- 6. Enable SSH (Remote Login) ------------------------------------------
echo "[6/7] Enabling SSH (Remote Login)..."
# This requires admin privileges
if sudo systemsetup -getremotelogin 2>/dev/null | grep -qi "on"; then
  echo "  ✓ Remote Login already enabled"
else
  sudo systemsetup -setremotelogin on 2>/dev/null || {
    echo "  ⚠ Could not enable SSH automatically."
    echo "  → Go to System Settings → General → Sharing → Remote Login → ON"
  }
fi

# ---- 7. Mount shared volume (9P) -------------------------------------------
echo "[7/7] Setting up 9P shared volume auto-mount..."
# Create a login script that mounts the shared volume
MOUNT_SCRIPT="$HOME/.arborist-mount-shared.sh"
cat > "$MOUNT_SCRIPT" << 'EOF'
#!/bin/bash
# Auto-mount the Docker shared volume (9P) if available
if [ ! -d /Volumes/shared/src-tauri ]; then
  sudo mkdir -p /Volumes/shared 2>/dev/null
  sudo mount_9p shared 2>/dev/null || true
fi
EOF
chmod +x "$MOUNT_SCRIPT"

# Add to login items via launchd plist
PLIST="$HOME/Library/LaunchAgents/io.arborist.mount-shared.plist"
mkdir -p "$HOME/Library/LaunchAgents"
cat > "$PLIST" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.arborist.mount-shared</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>${MOUNT_SCRIPT}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
</dict>
</plist>
EOF
echo "  ✓ 9P auto-mount configured"

# ---- Done -------------------------------------------------------------------
echo ""
echo "=== Provisioning complete ==="
echo ""
echo "Summary:"
echo "  • Xcode CLI Tools: $(xcode-select -p 2>/dev/null || echo 'pending')"
echo "  • Homebrew: $(brew --version 2>/dev/null | head -1 || echo 'pending')"
echo "  • Rust: $(rustc --version 2>/dev/null || echo 'pending')"
echo "  • Node: $(node --version 2>/dev/null || echo 'pending')"
echo "  • pnpm: $(pnpm --version 2>/dev/null || echo 'pending')"
echo "  • SSH: $(sudo systemsetup -getremotelogin 2>/dev/null || echo 'check manually')"
echo ""
echo "Next steps:"
echo "  1. Mount the shared volume: sudo mount_9p shared"
echo "  2. Verify source is at /Volumes/shared/src-tauri"
echo "  3. Run from the host: docker compose run --rm build"
echo ""
