# macOS e2e Test Harness

Manual Docker-based harness for verifying the **release-built** Arborist `.app` bundle
on macOS. Runs a full macOS VM inside Docker via QEMU/KVM using
[sickcodes/docker-osx](https://github.com/sickcodes/Docker-OSX) (default) or
[dockurr/macos](https://github.com/dockur/macos) (alternative).

> **Legal note**: Running macOS in a VM is only permitted on Apple hardware per
> Apple's EULA. Only use this harness on machines sold by Apple.

## Prerequisites

- **Linux host with KVM**, or **Windows 11** with Docker Desktop (WSL2 backend provides KVM)
- `/dev/kvm` accessible to Docker (check: `ls -la /dev/kvm`)
- Host tools: `sshpass`, `ssh`, `scp`, `rsync`
- ~20 GB disk for the macOS VM image
- First boot + provisioning takes 30–60 minutes (one-time)

### Install host tools

```bash
# Ubuntu/Debian/WSL2
sudo apt install sshpass openssh-client rsync

# macOS (if using a Mac host with nested virt)
brew install hudochenkov/sshpass/sshpass rsync
```

### Verify KVM support

```bash
# Linux
sudo apt install cpu-checker && sudo kvm-ok

# Windows 11 (in WSL2)
ls /dev/kvm   # should exist if Hyper-V is enabled
```

## Quick Start

```bash
# 1. Start the macOS VM (first run downloads + installs macOS — interactive)
pnpm e2e:macos:vm

# 2. Complete one-time provisioning (see "First-Time VM Setup" below)
pnpm e2e:macos:provision

# 3. Build the .app bundle inside the VM
pnpm e2e:macos:build

# 4. Run WebdriverIO e2e specs
pnpm e2e:macos

# 5. Drop into an interactive SSH shell
pnpm e2e:macos:shell

# 6. Check VM status
pnpm e2e:macos:status

# 7. Stop the VM
pnpm e2e:macos:down
```

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Docker Host (Linux / WSL2)                                 │
│                                                             │
│  ┌──────────────────────────────┐                           │
│  │  vm container (QEMU/KVM)     │                           │
│  │                              │                           │
│  │  ┌────────────────────────┐  │                           │
│  │  │  macOS Sonoma guest    │  │   ◄── SSH on port 50922   │
│  │  │                        │  │                           │
│  │  │  • Xcode CLI Tools     │  │                           │
│  │  │  • Rust + Node/pnpm   │  │                           │
│  │  │  • Builds .app bundle  │  │                           │
│  │  │  • Runs tauri-driver   │  │                           │
│  │  │  • Runs WebdriverIO    │  │                           │
│  │  └────────────────────────┘  │                           │
│  │           ▲ /dev/kvm         │                           │
│  └───────────┼──────────────────┘                           │
│              │                                              │
│  ┌───────────┴──────────────────────────────────────────┐   │
│  │  Host scripts (dev/e2e/macos/scripts/run.sh)         │   │
│  │  • Waits for VM boot                                 │   │
│  │  • rsync source → VM                                 │   │
│  │  • SSH commands: build, test, shell                   │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

### Docker image options

| Image | SSH Port | Notes |
|---|---|---|
| `sickcodes/docker-osx:latest` (default) | 10022 (QEMU hostfwd) → host 50922 | Pre-wired SSH, X11 forwarding, `sshfs` for files |
| `dockurr/macos` | 22 (after manual enable) → host 50922 | Web viewer on :8006, 9P file sharing |

Switch images:
```bash
# dockurr/macos exposes SSH on port 22 (after enabling), not 10022
MACOS_IMAGE=dockurr/macos MACOS_SSH_CONTAINER_PORT=22 pnpm e2e:macos:vm
```

## First-Time VM Setup

### Step 1: Start the VM

```bash
pnpm e2e:macos:vm
```

### Step 2: Complete macOS installation (interactive)

Connect to the VM's display:
- **sickcodes/docker-osx**: VNC on `localhost:5900`, or X11 forwarding
- **dockurr/macos**: http://localhost:8006

Complete the macOS installer:
1. Open Disk Utility → format the largest VirtIO disk as APFS
2. Install macOS to that disk
3. Create user account: username `arborist`, password `arborist`
4. Skip Apple ID, migration assistant, etc.

### Step 3: Enable SSH inside macOS

Once logged into the macOS desktop:
- System Settings → General → Sharing → Remote Login → ON

Or via Terminal.app inside the VM:
```bash
sudo systemsetup -setremotelogin on
```

### Step 4: Provision the VM

```bash
pnpm e2e:macos:provision
```

This installs Xcode CLI Tools, Homebrew, Rust, Node.js 22, pnpm, LLVM/LLD, and
tauri-driver. Takes ~15 minutes on first run.

### Step 5: Verify

```bash
pnpm e2e:macos:status
pnpm e2e:macos:shell
# Inside: rustc --version && node --version && pnpm --version
```

## Subsequent Runs

After provisioning, the VM disk persists in a Docker volume. Subsequent runs:

```bash
# Start VM (boots from disk in ~30s)
pnpm e2e:macos:vm

# Run full e2e (syncs source, builds, tests)
pnpm e2e:macos

# Or just build
pnpm e2e:macos:build
```

## Hermetic CLI Testing

Same as the Linux harness — no real `claude` or `copilot` CLIs are installed.
`arborist-test-child` is built alongside the app and passed via
`--ai-launch-claude` / `--ai-launch-copilot` flags.

## Shared Specs

The e2e specs (`01-launch.spec.ts`, helpers/) are reused from the Linux harness
at `dev/e2e/linux/specs/`. A macOS-specific `wdio.macos.conf.ts` adapts paths
and driver configuration for the macOS environment (SafariDriver via tauri-driver).

## Debugging

### Interactive shell

```bash
pnpm e2e:macos:shell
# Inside the VM:
~/arborist/target/release/bundle/macos/Arborist.app/Contents/MacOS/Arborist &
~/.cargo/bin/tauri-driver --port 4444 &
cd ~/e2e-specs && npx wdio run wdio.conf.ts --spec ./01-launch.spec.ts
```

### VNC access

Connect a VNC client to `localhost:5900`.

### Web viewer (dockurr/macos only)

Open http://localhost:8006.

## Troubleshooting

### "KVM device not found"

- Linux: `sudo modprobe kvm_intel` (or `kvm_amd`), check `/dev/kvm` permissions
- Windows 11: Enable Hyper-V, ensure WSL2 kernel supports KVM

### VM boot hangs

- First boot downloads macOS (~14 GB) — be patient
- Check logs: `docker compose -f dev/e2e/macos/docker-compose.yml logs vm`
- Ensure enough RAM (8 GB for VM + host overhead)

### SSH connection refused

- VM may still be booting (30–60s after container starts)
- Check SSH is enabled: connect via VNC and verify System Settings → Sharing
- Verify port with `pnpm e2e:macos:status`

### Build fails inside VM

- Ensure Xcode CLI Tools installed: `xcode-select -p`
- Ensure LLVM on PATH: `which lld`
- Re-provision: `pnpm e2e:macos:provision`

### Reset VM (nuclear option)

```bash
docker compose -f dev/e2e/macos/docker-compose.yml down
docker volume rm $(docker volume ls -q | grep macos-storage)
# Then start fresh: pnpm e2e:macos:vm
```
