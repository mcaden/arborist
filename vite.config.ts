import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';

function devPort(): number {
  const raw = process.env.ARBORIST_DEV_PORT;
  // Same validation rule as scripts/tauri-dev.mjs: integer in [1, 65535].
  // Reject `Number()`-coerced forms like "1e3" or " 42 " so both entrypoints
  // agree on what counts as a valid override.
  if (!raw || !/^\d+$/.test(raw)) return 1420;
  const n = Number(raw);
  return n >= 1 && n <= 65535 ? n : 1420;
}

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  clearScreen: false,
  server: {
    port: devPort(),
    strictPort: true,
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'es2022',
    sourcemap: true,
  },
});
