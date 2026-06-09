/// <reference types="vite/client" />

// Injected at build/test time via Vite's `define` (see vite.config.ts and vitest.config.ts),
// sourced from package.json's `version` field by scripts/read-app-version.ts.
declare const __APP_VERSION__: string;
