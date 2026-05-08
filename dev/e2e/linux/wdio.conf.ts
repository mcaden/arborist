// =============================================================================
// Arborist — WebdriverIO configuration for Linux e2e
//
// Drives the release-built AppImage via tauri-driver + WebKitWebDriver under
// Xvfb. The tauri-driver process is managed by the entrypoint/wdio hooks.
// =============================================================================

import { spawn, type ChildProcess } from "child_process";

let tauriDriver: ChildProcess | null = null;
let shuttingDown = false;

// Path to the extracted AppImage entry point (set by the Dockerfile)
const APP_BINARY = "/opt/arborist/AppRun";

export const config: WebdriverIO.Config = {
  runner: "local",
  hostname: "127.0.0.1",
  port: 4444,

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

    // Give tauri-driver a moment to bind to port 4444
    return new Promise<void>((resolve) => setTimeout(resolve, 500));
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
