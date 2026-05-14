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
//      kicks off `restore_all_sessions` asynchronously (see docs/runtime-flows.md#boot-and-restore).
//
// In-app workspace switches are handled entirely by
// `lib/workspace-switch.ts::changeWorkspace`: the backend now runs the
// new workspace's restore inline and returns the post-switch
// `{ config, sessions }` in the result, which `changeWorkspace`
// adopts atomically into the stores. No `workspace://changed` event
// listener is needed; PR5 removed it.
//
// While the boot effect runs, a `<BootSplash />` is shown. On any thrown
// error from the hydrate steps, an error overlay with a Reload button is
// shown instead.

import { useEffect, useMemo, useRef, useState } from 'react';

import { MainArea } from '@/components/MainArea';
import { NewSessionDialog } from '@/components/NewSessionDialog';
import { Sidebar } from '@/components/Sidebar';
import { ShellCommandTrustDialogHost } from '@/components/ShellCommandTrustDialog';
import { WorkspacePicker } from '@/components/WorkspacePicker';
import { initTerminalRouter } from '@/hooks/use-terminal';
import { subscribeToActivity, subscribeToMetrics, subscribeToStatus } from '@/lib/session-events';
import { subscribeToSubExited, subscribeToSubRestored, subscribeToSubStatus } from '@/lib/sub-session-events';
import { formatError, frontendReady } from '@/lib/tauri-bridge';
import { createBuiltinsRegistry, PluginRegistryProvider } from '@/plugins';
import { selectWorkspaceRoot, useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';
import { useSubSessionStore } from '@/store/sub-session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import { selectIsSwitching, useWorkspaceSwitchUiStore } from '@/store/workspace-switch-ui-store';
import { useWorktreePrepStore } from '@/store/worktree-prep-store';

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
      <p className="max-w-prose text-center text-sm text-slate-600 dark:text-slate-300">{message}</p>
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
  const registry = useMemo(() => createBuiltinsRegistry(), []);

  return (
    <PluginRegistryProvider registry={registry}>
      <AppInner />
      <ShellCommandTrustDialogHost />
    </PluginRegistryProvider>
  );
}

function AppInner(): JSX.Element {
  const [status, setStatus] = useState<BootStatus>('booting');
  const [error, setError] = useState<string | null>(null);

  useDarkModeFromSystem();

  useEffect(() => {
    let cancelled = false;
    let unlistenStatus: (() => void) | null = null;
    let unlistenActivity: (() => void) | null = null;
    let unlistenMetrics: (() => void) | null = null;
    let unlistenSubStatus: (() => void) | null = null;
    let unlistenSubExited: (() => void) | null = null;
    let unlistenSubRestored: (() => void) | null = null;
    let unlistenWorktreePrep: (() => void) | null = null;

    const boot = async (): Promise<void> => {
      try {
        await useConfigStore.getState().hydrate();
        if (cancelled) return;
        // Attach the event listeners BEFORE hydrating sessions/sub-sessions
        // so any status events emitted while the snapshot is in flight are
        // applied to the cache instead of being dropped on the floor.
        unlistenStatus = subscribeToStatus();
        unlistenActivity = subscribeToActivity();
        unlistenMetrics = subscribeToMetrics();
        unlistenSubStatus = subscribeToSubStatus();
        unlistenSubExited = subscribeToSubExited();
        // `subsession://restored` MUST be attached before `frontendReady()`
        // — the restore-on-launch second pass emits one event per
        // sub-session and the frontend store needs the row hydrated
        // before any subsequent status event can update it.
        unlistenSubRestored = subscribeToSubRestored();
        // `worktree://prep` is attached here too so even a sub-second prep
        // that exits before this effect runs again won't be lost. Issue #63.
        unlistenWorktreePrep = await useWorktreePrepStore.getState().subscribe();
        await useSessionStore.getState().actions.hydrate();
        if (cancelled) return;
        await useSubSessionStore.getState().actions.hydrate();
        if (cancelled) return;
        // Worktree-tab hydration runs AFTER session hydration so the self-heal step (open a tab for any session whose worktreePath has no
        // matching tab) sees a populated session list. Without this, an orphan session created before a previous crash would never render
        // under the new sidebar's worktree-keyed iteration. Hydrate's bridge errors propagate so App.boot's error overlay surfaces them.
        const knownPaths = useSessionStore.getState().sessions.map((s) => s.worktreePath);
        await useWorktreeTabStore.getState().actions.hydrate(knownPaths);
        if (cancelled) return;
        initTerminalRouter();
        if (cancelled) return;
        await frontendReady();
        if (cancelled) return;
        setStatus('ready');
      } catch (err) {
        if (cancelled) return;
        const message = formatError(err);
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
      if (unlistenSubStatus) {
        try {
          unlistenSubStatus();
        } catch {
          // ignore
        }
      }
      if (unlistenSubExited) {
        try {
          unlistenSubExited();
        } catch {
          // ignore
        }
      }
      if (unlistenSubRestored) {
        try {
          unlistenSubRestored();
        } catch {
          // ignore
        }
      }
      if (unlistenWorktreePrep) {
        try {
          unlistenWorktreePrep();
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

  // The `isSwitching` subscription lives inside `ReadyAppShell` (not
  // here) so the first-boot picker branch above doesn't re-render on
  // flag flips. In practice the picker only mounts when no workspace
  // is bound and `changeWorkspace` (the only writer) is unreachable
  // from there, so this is hygiene rather than a live bug.
  return <ReadyAppShell />;
}

// Split out so the focus-management `useEffect` only mounts under the
// `workspaceRoot` branch (the picker branch returns early above and
// must not register the trap). Two layers gate input while a
// transactional workspace switch is in flight (see docs/runtime-flows.md#workspace-switching -
// inputs received mid-switch would land against ambiguous state):
//
// 1. The underlying app root gets `aria-busy` and `inert`. `inert`
//    is the authoritative gate: it removes the subtree from the
//    sequential focus order AND blocks click / keyboard / AT
//    interactions. All Tauri-supported WebViews ship with `inert`
//    today (WebView2 ≥109, WKWebView ≥15.5, WebKitGTK ≥2.40), so
//    the previous claim that `pointer-events-auto` was a "failsafe
//    even on platforms where `inert` is unavailable" was misleading
//    — `pointer-events-auto` only addresses pointer events, not
//    keyboard.
// 2. The overlay itself is a modal: `role="alertdialog"`,
//    `aria-modal="true"`, `tabIndex={-1}`, and an effect moves
//    focus into it on mount (so any element previously focused in
//    the now-inert subtree loses focus) and restores focus on
//    unmount. A document-level `focusin` listener bounces escapes
//    back into the overlay as a defence-in-depth in the (currently
//    hypothetical) case where a future WebView regression lets
//    focus escape `inert`.
//
// The `isSwitching` flag flips off in the same render that adopts
// the new workspace's data, so users never see a "no workspace"
// flash between hide-overlay and tabs-populated.
function ReadyAppShell(): JSX.Element {
  const isSwitching = useWorkspaceSwitchUiStore(selectIsSwitching);
  const overlayRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (!isSwitching) return;
    const previouslyFocused = typeof document !== 'undefined' ? (document.activeElement as HTMLElement | null) : null;
    overlayRef.current?.focus();
    const trapFocus = (e: FocusEvent): void => {
      const overlay = overlayRef.current;
      if (!overlay) return;
      const target = e.target as Node | null;
      if (target && !overlay.contains(target)) {
        overlay.focus();
      }
    };
    document.addEventListener('focusin', trapFocus);
    return () => {
      document.removeEventListener('focusin', trapFocus);
      // Only restore focus if the previously-focused element is still
      // attached and focusable. After a successful switch the old
      // session's tab is gone from the DOM, so there's nothing to
      // restore — let the browser pick the next focus target.
      if (previouslyFocused && typeof previouslyFocused.focus === 'function' && document.contains(previouslyFocused)) {
        previouslyFocused.focus();
      }
    };
  }, [isSwitching]);

  return (
    <div className="relative h-full w-full">
      <div
        className="flex h-full w-full bg-white text-slate-900 dark:bg-slate-900 dark:text-slate-100"
        aria-busy={isSwitching || undefined}
        inert={isSwitching || undefined}
      >
        <Sidebar />
        <MainArea />
        <NewSessionDialog />
      </div>
      {isSwitching && (
        <div
          ref={overlayRef}
          role="alertdialog"
          aria-modal="true"
          aria-labelledby="workspace-switch-overlay-label"
          aria-live="polite"
          tabIndex={-1}
          data-testid="workspace-switch-overlay"
          className="pointer-events-auto absolute inset-0 z-50 flex items-center justify-center bg-white/70 outline-none backdrop-blur-sm dark:bg-slate-900/70"
        >
          <div className="flex flex-col items-center gap-2 rounded border border-slate-300 bg-white px-6 py-4 text-sm text-slate-700 shadow dark:border-slate-700 dark:bg-slate-800 dark:text-slate-200">
            <p id="workspace-switch-overlay-label">Switching workspace…</p>
          </div>
        </div>
      )}
    </div>
  );
}
