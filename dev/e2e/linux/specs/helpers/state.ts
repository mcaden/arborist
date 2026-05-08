// =============================================================================
// State pre-seeding helpers for e2e specs.
//
// Writes AppConfig JSON directly to the expected config directory so tests
// can start with a known state (e.g., for restore-on-launch tests).
// =============================================================================

import fs from "fs";
import os from "os";
import path from "path";

/**
 * Returns the path where tauri-plugin-store persists data on Linux.
 * XDG_CONFIG_HOME defaults to ~/.config; Tauri uses the app identifier.
 */
export function getConfigDir(): string {
  const home = process.env.HOME || os.homedir();
  const xdgConfig = process.env.XDG_CONFIG_HOME || path.join(home, ".config");
  return path.join(xdgConfig, "dev.arborist.desktop");
}

/**
 * Pre-seed the AppConfig store file with the given data.
 * Useful for tests that need to start with existing sessions or instruction sets.
 */
export function seedAppConfig(config: Record<string, unknown>): void {
  const configDir = getConfigDir();
  fs.mkdirSync(configDir, { recursive: true });
  fs.writeFileSync(path.join(configDir, "config.json"), JSON.stringify(config, null, 2));
}

/**
 * Clear all persisted state so the next app launch starts fresh.
 */
export function clearAppState(): void {
  const configDir = getConfigDir();
  if (fs.existsSync(configDir)) {
    fs.rmSync(configDir, { recursive: true, force: true });
  }
}
