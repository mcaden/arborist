// MainArea — right-hand pane of the app shell.
//
// Renders **every** session's TerminalView simultaneously, hiding all but
// the active one via inline `display: none`. The motivation is twofold:
//
//   1. xterm Terminal instances live in the `use-terminal` module-level
//      registry, so even if MainArea swapped TerminalViews on tab change
//      the underlying Terminal would survive — but only because of that
//      indirection. Rendering all views means each Terminal is `attach`'d
//      exactly once per session lifetime, which sidesteps any reattachment
//      flicker and makes the lifecycle trivially reasonable.
//   2. Keeps SPEC T-03 (scrollback persists across tab switches) honest:
//      hidden views still receive `session://output` (because the listener
//      is keyed by sessionId in the registry, not by mount status) and
//      keep their xterm scrollback intact.

import { TerminalView } from './TerminalView';
import { useActiveSessionId, useSessions } from '@/store/session-store';

export function MainArea(): JSX.Element {
  const sessions = useSessions();
  const activeId = useActiveSessionId();

  if (sessions.length === 0) {
    return (
      <main className="flex h-full min-w-0 flex-1 items-center justify-center bg-white text-slate-700 dark:bg-slate-950 dark:text-slate-200">
        <p className="text-sm text-slate-400">No session selected — create one to begin.</p>
      </main>
    );
  }

  return (
    <main className="relative flex h-full min-w-0 flex-1 bg-black">
      {sessions.map((session) => {
        const active = session.id === activeId;
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
    </main>
  );
}
