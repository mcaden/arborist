// =============================================================================
// Arborist — WebdriverIO configuration for macOS e2e
//
// Drives the release-built .app bundle via tauri-driver + SafariDriver on
// macOS. tauri-driver is managed by the wdio hooks (same pattern as Linux).
// =============================================================================

import { spawn, type ChildProcess } from "child_process";
import { createConnection, type Socket } from "net";
import { existsSync } from "fs";
import { resolve } from "path";

let tauriDriver: ChildProcess | null = null;
let shuttingDown = false;

// Locate the .app bundle — check 9P shared mount first, fallback to ~/arborist
const SHARED_BUNDLE = "/Volumes/shared/target/release/bundle/macos/Arborist.app";
const LOCAL_BUNDLE = resolve(process.env.HOME ?? "~", "arborist/target/release/bundle/macos/Arborist.app");
const APP_BINARY = existsSync(SHARED_BUNDLE)
  ? `${SHARED_BUNDLE}/Contents/MacOS/Arborist`
  : `${LOCAL_BUNDLE}/Contents/MacOS/Arborist`;

// Locate arborist-test-child
const SHARED_TEST_CHILD = "/Volumes/shared/target/release/arborist-test-child";
const LOCAL_TEST_CHILD = resolve(process.env.HOME ?? "~", "arborist/target/release/arborist-test-child");
const TEST_CHILD = existsSync(SHARED_TEST_CHILD) ? SHARED_TEST_CHILD : LOCAL_TEST_CHILD;

const TEST_WORKSPACE = "/tmp/arborist-test-workspace";
const DRIVER_PORT = 4444;
const DRIVER_STARTUP_TIMEOUT_MS = 10_000;
const DRIVER_POLL_INTERVAL_MS = 100;

/** Poll until a TCP connection to localhost:port succeeds, or timeout. */
function waitForPort(port: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  return new Promise<void>((resolve, reject) => {
    function attempt() {
      if (Date.now() > deadline) {
        reject(new Error(`tauri-driver did not bind to port ${port} within ${timeoutMs}ms`));
        return;
      }
      const sock: Socket = createConnection({ port, host: "127.0.0.1" }, () => {
        sock.destroy();
        resolve();
      });
      sock.on("error", () => {
        sock.destroy();
        setTimeout(attempt, DRIVER_POLL_INTERVAL_MS);
      });
    }
    attempt();
  });
}

export const config: WebdriverIO.Config = {
  runner: "local",
  hostname: "127.0.0.1",
  port: DRIVER_PORT,

  specs: ["./**/*.spec.ts"],
  exclude: ["./helpers/**"],

  maxInstances: 1,

  capabilities: [
    {
      maxInstances: 1,
      "tauri:options": {
        application: APP_BINARY,
        args: [
          "--workspace",
          TEST_WORKSPACE,
          `--ai-launch-claude=${TEST_CHILD}`,
          `--ai-launch-copilot=${TEST_CHILD}`,
        ],
      },
    },
  ],

  logLevel: "warn",
  waitforTimeout: 10000,
  connectionRetryTimeout: 120000,
  connectionRetryCount: 3,

  framework: "mocha",
  reporters: ["spec"],

  mochaOpts: {
    ui: "bdd",
    timeout: 60000,
  },

  beforeSession: () => {
    // On macOS, tauri-driver uses SafariDriver under the hood.
    // It must be installed via: cargo install tauri-cli (provides tauri-driver)
    const driverPath = resolve(process.env.HOME ?? "~", ".cargo/bin/tauri-driver");

    tauriDriver = spawn(driverPath, ["--port", String(DRIVER_PORT)], {
      stdio: [null, process.stdout, process.stderr],
    });

    tauriDriver.on("error", (error) => {
      console.error("[wdio] tauri-driver error:", error);
      if (!shuttingDown) process.exit(1);
    });

    tauriDriver.on("exit", (code) => {
      if (!shuttingDown) {
        console.error("[wdio] tauri-driver exited unexpectedly with code:", code);
        process.exit(1);
      }
    });

    return waitForPort(DRIVER_PORT, DRIVER_STARTUP_TIMEOUT_MS);
  },

  afterSession: () => {
    shuttingDown = true;
    tauriDriver?.kill();
    tauriDriver = null;
  },
};

// Cleanup on unexpected exit
function cleanup() {
  shuttingDown = true;
  try {
    tauriDriver?.kill();
  } catch {
    // ignore
  }
}

process.on("exit", cleanup);
process.on("SIGINT", () => {
  cleanup();
  process.exit(130);
});
process.on("SIGTERM", () => {
  cleanup();
  process.exit(143);
});
