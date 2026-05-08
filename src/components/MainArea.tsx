// MainArea — right-hand pane of the app shell.
//
// Renders **every** session's TerminalView simultaneously, plus every
// **terminal** sub-session's SubTerminalView, hiding all but the visible
// one via `visibility: hidden`. The motivation is twofold:
//
//   1. xterm Terminal instances live in the `use-terminal` module-level
//      registry, so even if MainArea swapped views on tab change the
//      underlying Terminal would survive — but only because of that
//      indirection. Rendering all views means each Terminal is `attach`'d
//      exactly once per session lifetime, which sidesteps any reattachment
//      flicker and makes the lifecycle trivially reasonable.
//   2. Keeps SPEC T-03 (scrollback persists across tab switches) honest:
//      hidden views still receive `session://output` (because the listener
//      is keyed by id in the registry, not by mount status) and keep their
//      xterm scrollback intact.
//
// Visible-id derivation (issue #44):
//   * If no worktree tabs exist → empty placeholder.
//   * Otherwise we read `(activeWorktreeTabId, tab.activeChildId)` as the
//     single source of truth:
//       - `activeChildId` undefined / null     → `<WorktreeDashboard>`.
//       - `activeChildId.kind === 'session'`   → that session's terminal.
//       - `activeChildId.kind === 'subSession'` → that sub-session's
//         terminal directly (sub-sessions are now owned by worktree tabs,
//         not by agent sessions).

import { SubTerminalView } from './SubTerminalView';
import { TerminalView } from './TerminalView';
import { WorktreeDashboard } from './WorktreeDashboard';
import { useSessions } from '@/store/session-store';
import { useAllSubSessions } from '@/store/sub-session-store';
import { useActiveWorktreeTabId, useWorktreeTabs } from '@/store/worktree-tab-store';
import type { SessionId, SubSessionId } from '@/types/arborist';

export function MainArea(): JSX.Element {
  const sessions = useSessions();
  const worktreeTabs = useWorktreeTabs();
  const activeWorktreeTabId = useActiveWorktreeTabId();
  const allSubs = useAllSubSessions();

  const activeWorktreeTab = worktreeTabs.find((t) => t.id === activeWorktreeTabId) ?? null;

  // Resolve the active session id and visible sub-session id from worktree-tab activeChildId. Stale or invalid children deliberately fall
  // back to the dashboard instead of leaving every terminal wrapper hidden.
  let activeSessionId: SessionId | undefined;
  let visibleSubId: SubSessionId | undefined;
  if (activeWorktreeTab) {
    const child = activeWorktreeTab.activeChildId;
    if (child?.kind === 'session') {
      const session = sessions.find((s) => s.id === child.id && s.worktreePath === activeWorktreeTab.path);
      if (session) activeSessionId = session.id;
    } else if (child?.kind === 'subSession') {
      const sub = allSubs.find((s) => s.id === child.id && s.parentWorktreeTabId === activeWorktreeTab.id);
      if (sub && sub.kind === 'terminal') visibleSubId = sub.id;
    }
  }

  const showDashboard =
    worktreeTabs.length > 0 &&
    activeWorktreeTab !== null &&
    (activeWorktreeTab.activeChildId === undefined ||
      activeWorktreeTab.activeChildId === null ||
      (activeWorktreeTab.activeChildId.kind === 'session' && activeSessionId === undefined) ||
      (activeWorktreeTab.activeChildId.kind === 'subSession' && visibleSubId === undefined));

  if (sessions.length === 0 && worktreeTabs.length === 0) {
    return (
      <main className="flex h-full min-w-0 flex-1 items-center justify-center bg-white text-slate-700 dark:bg-slate-950 dark:text-slate-200">
        <p className="text-sm text-slate-400">No session selected — create one to begin.</p>
      </main>
    );
  }

  const terminalSubs = allSubs.filter((s) => s.kind === 'terminal');

  return (
    <main className="relative flex h-full min-w-0 flex-1 bg-black">
      {sessions.map((session) => {
        const active = !showDashboard && visibleSubId === undefined && session.id === activeSessionId;
        return (
          <div
            key={session.id}
            className="absolute inset-0"
            style={
              active
                ? undefined
                : {
                    visibility: 'hidden',
                    pointerEvents: 'none',
                  }
            }
            aria-hidden={!active}
          >
            <TerminalView sessionId={session.id} isActive={active} />
          </div>
        );
      })}
      {terminalSubs.map((sub) => {
        const active = !showDashboard && sub.id === visibleSubId;
        return (
          <div
            key={sub.id}
            className="absolute inset-0"
            style={
              active
                ? undefined
                : {
                    visibility: 'hidden',
                    pointerEvents: 'none',
                  }
            }
            aria-hidden={!active}
          >
            <SubTerminalView subSessionId={sub.id} isActive={active} />
          </div>
        );
      })}
      {showDashboard && activeWorktreeTab && <WorktreeDashboard tabId={activeWorktreeTab.id} />}
    </main>
  );
}
