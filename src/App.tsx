// App shell. Phase 9 wires in the Sidebar; the right-hand pane is a
// **placeholder** until Phase 11/12 mount the real <TerminalView />.

import { Sidebar } from '@/components/Sidebar';
import { useActiveSession } from '@/store/session-store';

function MainAreaPlaceholder(): JSX.Element {
  // PHASE 11 TODO: replace with <TerminalView />.
  const active = useActiveSession();
  return (
    <main className="flex h-full min-w-0 flex-1 items-center justify-center bg-white text-slate-700 dark:bg-slate-950 dark:text-slate-200">
      {active ? (
        <p className="text-sm">Active session: {active.label}</p>
      ) : (
        <p className="text-sm text-slate-400">No session selected</p>
      )}
    </main>
  );
}

export function App(): JSX.Element {
  return (
    <div className="flex h-full w-full bg-white text-slate-900 dark:bg-slate-900 dark:text-slate-100">
      <Sidebar />
      <MainAreaPlaceholder />
    </div>
  );
}
