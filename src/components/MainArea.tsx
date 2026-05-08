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
//      xterm scrollback intact. The same rule applies to terminal
//      sub-sessions, whose output flows through the same channel.
//
// Visible-id derivation:
//   * Default: the active parent session.
//   * If `activeByParent[activeId]` points at a **terminal** sub-session,
//     that sub-session is shown instead. Application sub-sessions never
//     swap the viewport (they get OS-window focus only — see
//     `sub-session-store.focus`).

import { SubTerminalView } from './SubTerminalView';
import { TerminalView } from './TerminalView';
import { useActiveSessionId, useSessions } from '@/store/session-store';
import { useActiveSubSessionId, useAllSubSessions } from '@/store/sub-session-store';

export function MainArea(): JSX.Element {
  const sessions = useSessions();
  const activeId = useActiveSessionId();
  const activeSubForActiveParent = useActiveSubSessionId(activeId);
  const allSubs = useAllSubSessions();

  if (sessions.length === 0) {
    return (
      <main
        data-testid="main-area"
        className="flex h-full min-w-0 flex-1 items-center justify-center bg-white text-slate-700 dark:bg-slate-950 dark:text-slate-200"
      >
        <p className="text-sm text-slate-400">No session selected — create one to begin.</p>
      </main>
    );
  }

  // Resolve the visible terminal: a terminal sub-session if `activeByParent`
  // points to one for the active parent, else the parent session itself.
  // We lookup the candidate sub-session by id to confirm it is (still) a
  // terminal kind — application kinds never swap the viewport.
  const activeSub = activeSubForActiveParent ? allSubs.find((s) => s.id === activeSubForActiveParent) : undefined;
  const visibleSubId = activeSub && activeSub.kind === 'terminal' ? activeSub.id : undefined;

  // All terminal sub-sessions get mounted (hidden) so their xterm
  // scrollback keeps accumulating output, matching parent-session behaviour.
  const terminalSubs = allSubs.filter((s) => s.kind === 'terminal');

  return (
    <main data-testid="main-area" className="relative flex h-full min-w-0 flex-1 bg-black">
      {sessions.map((session) => {
        const active = visibleSubId === undefined && session.id === activeId;
        return (
          <div
            key={session.id}
            className="absolute inset-0"
            style={
              active
                ? undefined
                : {
                    // Keep hidden panels laid out so xterm.js can measure
                    // character dimensions correctly on first open and so
                    // `fitAddon.fit()` has a real container to size against.
                    // `display: none` zeroes the box and produces a
                    // "squished" banner once the tab is shown again because
                    // the PTY had already emitted output against bogus
                    // metrics.
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
        const active = sub.id === visibleSubId;
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
    </main>
  );
}
