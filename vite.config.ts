import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { relative, isAbsolute } from 'node:path';

// Default dev port. The actual per-worktree port is supplied by
// `scripts/tauri-dev.mjs` via vite's `--port=<n>` CLI flag (see Vite's
// preview/server options) — kept out of this config so the codebase carries
// no project-specific environment variables.
const DEFAULT_DEV_PORT = 1420;

const projectRoot = fileURLToPath(new URL('.', import.meta.url));

// Ignore `.worktrees/` directories that live *inside* the Vite project root
// without matching `.worktrees` segments in the project root's own absolute
// path. A naive `'**/.worktrees/**'` glob is unsafe here: when the dev server
// is itself launched from a linked worktree (e.g. `<repo>/.worktrees/foo/`),
// every absolute file path contains `.worktrees` as an ancestor segment, so
// chokidar would ignore the entire source tree and silently break HMR.
function ignoreNestedWorktrees(file: string): boolean {
  const rel = relative(projectRoot, file);
  if (!rel || rel.startsWith('..') || isAbsolute(rel)) return false;
  // Split on either separator: `path.relative` uses the platform separator
  // (`\` on Windows, `/` on POSIX), but chokidar can hand us paths with
  // forward slashes on Windows too — handle both.
  return rel.split(/[\\/]/).includes('.worktrees');
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
    port: DEFAULT_DEV_PORT,
    strictPort: true,
    watch: {
      // Arborist creates linked git worktrees under `<workspaceRoot>/.worktrees/<name>/`.
      // When the workspace root is the Vite project root, each new worktree
      // dumps thousands of files into chokidar's tree and triggers a full HMR
      // reload. See `ignoreNestedWorktrees` for why this is path-anchored
      // rather than a `**/.worktrees/**` glob.
      ignored: [ignoreNestedWorktrees],
    },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'es2022',
    sourcemap: true,
  },
});
