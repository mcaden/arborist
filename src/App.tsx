// App shell. Phase 12 owns the boot sequence:
//
//   1. Hydrate the config-store from the backend.
//   2. Hydrate the session-store from `session_list` (the persisted snapshot
//      sorted by tabIndex — statuses return as-last-persisted; the
//      `restore_all_sessions` step below flips each one to `Starting`
//      before respawn, then to `Running` / `Exited` / `Error` as the wait
//      thread observes the child).
//   3. `initTerminalRouter()` — attach the global `session://output` router.
//   4. `subscribeToStatus()` — attach the global `session://status` router.
//   5. `frontendReady()` — tell the backend listeners are live; backend then
//      kicks off `restore_all_sessions` asynchronously (DESIGN §5.5).
//
// While the boot effect runs, a `<BootSplash />` is shown. On any thrown
// error from the hydrate steps, an error overlay with a Reload button is
// shown instead.

import { useEffect, useState } from 'react';

import { MainArea } from '@/components/MainArea';
import { NewSessionDialog } from '@/components/NewSessionDialog';
import { Sidebar } from '@/components/Sidebar';
import { WorkspacePicker } from '@/components/WorkspacePicker';
import { initTerminalRouter } from '@/hooks/use-terminal';
import { subscribeToActivity, subscribeToMetrics, subscribeToStatus } from '@/lib/session-events';
import { frontendReady, onWorkspaceChanged } from '@/lib/tauri-bridge';
import { rehydrateActiveWorkspace } from '@/lib/rehydrate-workspace';
import { selectWorkspaceRoot, useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

type BootStatus = 'booting' | 'ready' | 'error';

function applyDarkModeClass(isDark: boolean): void {
  if (typeof document === 'undefined') return;
  const root = document.documentElement;
  if (isDark) {
    root.classList.add('dark');
  } else {
    root.classList.remove('dark');
  }
}

function useDarkModeFromSystem(): void {
  useEffect(() => {
    if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
      return;
    }
    const mq = window.matchMedia('(prefers-color-scheme: dark)');
    applyDarkModeClass(mq.matches);
    const onChange = (e: MediaQueryListEvent): void => applyDarkModeClass(e.matches);
    // Older WebViews only expose addListener/removeListener; prefer the
    // modern API and fall back where needed.
    if (typeof mq.addEventListener === 'function') {
      mq.addEventListener('change', onChange);
      return () => mq.removeEventListener('change', onChange);
    }
    mq.addListener(onChange);
    return () => mq.removeListener(onChange);
  }, []);
}

function BootSplash(): JSX.Element {
  return (
    <div
      role="status"
      aria-live="polite"
      className="flex h-full w-full items-center justify-center bg-white text-slate-700 dark:bg-slate-900 dark:text-slate-200"
    >
      <p className="text-sm">Loading Arborist…</p>
    </div>
  );
}

function ErrorOverlay({ message }: { message: string }): JSX.Element {
  return (
    <div
      role="alert"
      className="flex h-full w-full flex-col items-center justify-center gap-4 bg-white p-8 text-slate-900 dark:bg-slate-900 dark:text-slate-100"
    >
      <h1 className="text-lg font-semibold">Arborist failed to start</h1>
      <p className="max-w-prose text-center text-sm text-slate-600 dark:text-slate-300">
        {message}
      </p>
      <button
        type="button"
        onClick={() => window.location.reload()}
        className="rounded border border-slate-300 bg-slate-100 px-4 py-2 text-sm hover:bg-slate-200 dark:border-slate-700 dark:bg-slate-800 dark:hover:bg-slate-700"
      >
        Reload
      </button>
    </div>
  );
}

export function App(): JSX.Element {
  const [status, setStatus] = useState<BootStatus>('booting');
  const [error, setError] = useState<string | null>(null);

  useDarkModeFromSystem();

  useEffect(() => {
    let cancelled = false;
    let unlistenStatus: (() => void) | null = null;
    let unlistenActivity: (() => void) | null = null;
    let unlistenMetrics: (() => void) | null = null;
    let unlistenWorkspaceChanged: (() => void) | null = null;
    // `rehydrateActiveWorkspace` (in `lib/rehydrate-workspace.ts`)
    // owns the monotonic generation counter — it is shared between
    // this listener and the `changeWorkspace` fallback path so a
    // duplicate rehydrate (one from the event, one from the
    // post-`workspaceSwitch` fallback) is race-safe: whichever call
    // bumps the counter last wins, the other bails after its first
    // await.

    const boot = async (): Promise<void> => {
      try {
        await useConfigStore.getState().hydrate();
        if (cancelled) return;
        await useSessionStore.getState().actions.hydrate();
        if (cancelled) return;
        initTerminalRouter();
        unlistenStatus = subscribeToStatus();
        unlistenActivity = subscribeToActivity();
        unlistenMetrics = subscribeToMetrics();
        // Re-hydrate config + sessions and re-issue frontend_ready when the
        // backend swaps to a different workspace at runtime (Phase 7
        // in-app workspace switch). The backend has already (a) **parked**
        // every old-workspace session — PTYs killed but the persisted
        // session records (sessions.json, lastOpenSessions, tabOrder)
        // are intentionally retained so a later switch-back can restore
        // them — (b) bound the new (branch, workspace) lock, and (c)
        // persisted the new workspace_root into the active store before
        // emitting this event — so it is safe to simply re-fetch.
        unlistenWorkspaceChanged = await onWorkspaceChanged(() => {
          void rehydrateActiveWorkspace().catch((err) => {
            // The backend swap already succeeded; rehydration failure
            // here is a frontend-only inconsistency and shouldn't
            // crash the app. Surface to console so the user can
            // reload manually if needed.
            console.error('workspace://changed re-hydrate failed', err);
          });
        });
        await frontendReady();
        if (cancelled) return;
        setStatus('ready');
      } catch (err) {
        if (cancelled) return;
        const message = err instanceof Error ? err.message : String(err);
        setError(message);
        setStatus('error');
      }
    };

    void boot();

    return () => {
      cancelled = true;
      if (unlistenStatus) {
        try {
          unlistenStatus();
        } catch {
          // ignore
        }
      }
      if (unlistenActivity) {
        try {
          unlistenActivity();
        } catch {
          // ignore
        }
      }
      if (unlistenMetrics) {
        try {
          unlistenMetrics();
        } catch {
          // ignore
        }
      }
      if (unlistenWorkspaceChanged) {
        try {
          unlistenWorkspaceChanged();
        } catch {
          // ignore
        }
      }
    };
  }, []);

  if (status === 'error') {
    return <ErrorOverlay message={error ?? 'Unknown error'} />;
  }
  if (status === 'booting') {
    return <BootSplash />;
  }

  return <ReadyApp />;
}

function ReadyApp(): JSX.Element {
  const workspaceRoot = useConfigStore(selectWorkspaceRoot);
  const setConfig = useConfigStore((s) => s.set);

  if (workspaceRoot === null || workspaceRoot.length === 0) {
    return (
      <WorkspacePicker
        mode="first-boot"
        onConfirm={async (path) => {
          await setConfig({ workspaceRoot: path });
        }}
      />
    );
  }

  return (
    <div className="flex h-full w-full bg-white text-slate-900 dark:bg-slate-900 dark:text-slate-100">
      <Sidebar />
      <MainArea />
      <NewSessionDialog />
    </div>
  );
}
