import { readFileSync } from 'node:fs';
import { fileURLToPath, URL } from 'node:url';

// Single source of truth for the app version exposed to the frontend as `__APP_VERSION__`.
// Both vite.config.ts and vitest.config.ts inject this so the build and test bundles agree.
export function readAppVersion(): string {
  const pkg = JSON.parse(readFileSync(fileURLToPath(new URL('../package.json', import.meta.url)), 'utf-8')) as { version?: unknown };
  if (typeof pkg.version !== 'string' || pkg.version.length === 0) {
    throw new Error('package.json is missing a "version" field; cannot inject __APP_VERSION__');
  }
  return pkg.version;
}
