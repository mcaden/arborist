// Sidebar header showing the current workspace + a "Change workspace…"
// trigger — Roadmap §1.2, §1.3.
//
// Clicking "Change…" opens an in-app workspace picker; on confirm, every
// open session is closed and `workspaceRoot` is rewritten in config.

import { useCallback, useState } from 'react';

import { WorkspacePicker } from './WorkspacePicker';
import { changeWorkspace } from '@/lib/workspace-switch';
import { selectWorkspaceRoot, useConfigStore } from '@/store/config-store';

/** Extract the trailing path component, handling both POSIX and Windows separators. */
function basename(p: string): string {
  const trimmed = p.replace(/[\\/]+$/, '');
  const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
  return idx === -1 ? trimmed : trimmed.slice(idx + 1);
}

export function WorkspaceIndicator(): JSX.Element | null {
  const workspaceRoot = useConfigStore(selectWorkspaceRoot);
  const [picking, setPicking] = useState(false);

  const handleConfirm = useCallback(async (path: string) => {
    await changeWorkspace(path);
    setPicking(false);
  }, []);

  if (workspaceRoot === null || workspaceRoot.length === 0) {
    // App-level shell shows the first-boot picker in this state, so the
    // sidebar should not render at all.
    return null;
  }

  return (
    <>
      <div className="flex items-center gap-2 border-b border-slate-200 px-3 py-2 text-xs dark:border-slate-800" data-testid="workspace-indicator">
        <div className="min-w-0 flex-1">
          <p className="text-[10px] uppercase tracking-wide text-slate-500 dark:text-slate-400" id="workspace-label">
            Workspace
          </p>
          <p className="truncate text-sm font-medium text-slate-800 dark:text-slate-100" title={workspaceRoot} aria-labelledby="workspace-label">
            {basename(workspaceRoot)}
          </p>
        </div>
        <button
          type="button"
          onClick={() => setPicking(true)}
          className="shrink-0 rounded border border-slate-300 bg-white px-2 py-1 text-xs hover:bg-slate-100 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
        >
          Change…
        </button>
      </div>
      {picking ? (
        <div className="fixed inset-0 z-40 bg-black/40">
          <WorkspacePicker mode="change" initialPath={workspaceRoot} onConfirm={handleConfirm} onCancel={() => setPicking(false)} />
        </div>
      ) : null}
    </>
  );
}
