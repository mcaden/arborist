import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { relative, isAbsolute } from 'node:path';

function devPort(): number {
  const raw = process.env.ARBORIST_DEV_PORT;
  // Same validation rule as scripts/tauri-dev.mjs: integer in [1, 65535].
  // Reject `Number()`-coerced forms like "1e3" or " 42 " so both entrypoints
  // agree on what counts as a valid override.
  if (!raw || !/^\d+$/.test(raw)) return 1420;
  const n = Number(raw);
  return n >= 1 && n <= 65535 ? n : 1420;
}

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
    port: devPort(),
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
