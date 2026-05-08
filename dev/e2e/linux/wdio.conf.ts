// =============================================================================
// Arborist — WebdriverIO configuration for Linux e2e
//
// Drives the release-built AppImage via tauri-driver + WebKitWebDriver under
// Xvfb. The tauri-driver process is managed by the entrypoint/wdio hooks.
// =============================================================================

import { spawn, type ChildProcess } from "child_process";
import { createConnection, type Socket } from "net";

let tauriDriver: ChildProcess | null = null;
let shuttingDown = false;

// Path to the extracted AppImage entry point (set by the Dockerfile)
const APP_BINARY = "/opt/arborist/AppRun";
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

  specs: ["/specs/specs/**/*.spec.ts"],
  exclude: ["/specs/specs/helpers/**"],

  maxInstances: 1,

  capabilities: [
    {
      maxInstances: 1,
      "tauri:options": {
        application: APP_BINARY,
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

  // Start tauri-driver before the session so it can proxy WebDriver requests
  beforeSession: () => {
    const driverPath = "/usr/local/bin/tauri-driver";

    tauriDriver = spawn(driverPath, [], {
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

    // Wait for tauri-driver to actually bind to the port
    return waitForPort(DRIVER_PORT, DRIVER_STARTUP_TIMEOUT_MS);
  },

  afterSession: () => {
    shuttingDown = true;
    tauriDriver?.kill();
    tauriDriver = null;
  },
};

// Ensure cleanup on unexpected exit
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
