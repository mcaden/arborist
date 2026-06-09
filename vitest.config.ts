import { defineConfig, configDefaults } from 'vitest/config';
import react from '@vitejs/plugin-react';
import path from 'node:path';
import { readAppVersion } from './scripts/read-app-version';

const appVersion = readAppVersion();

export default defineConfig({
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(appVersion),
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    css: false,
    // Keep fork-worker startup under Windows hook timeouts; higher auto-detected parallelism intermittently fails before tests run.
    maxWorkers: 4,
    exclude: [...configDefaults.exclude, '**/.worktrees/**', 'dev/e2e/**'],
  },
});
