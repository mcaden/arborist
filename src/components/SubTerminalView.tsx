// SubTerminalView — mirrors `TerminalView` for terminal-kind sub-sessions.
// Hosts an xterm.js Terminal in the main area for a sub-session. Lifecycle
// rules match `TerminalView`: the underlying Terminal lives in the
// `use-terminal` registry and survives hide/show; this component only
// owns DOM mount/unmount and focus on activation.
//
// Status (running / exited / error) is communicated by the sidebar
// indicator, NOT by an in-pane overlay. When a sub-session exits
// outside the user's control, the sidebar dot turns grey and the user
// can click that sidebar tab to relaunch in place. This component
// deliberately renders no dialog / banner / overlay so the user can
// continue to read the final scrollback (e.g. an error message or
// `exit` echo) without a modal blocking it.

import { useEffect, useRef } from 'react';

import { useSubTerminal } from '@/hooks/use-terminal';
import { useSubSessionById } from '@/store/sub-session-store';
import type { SubSessionId } from '@/types/arborist';

interface SubTerminalViewProps {
  subSessionId: SubSessionId;
  /**
   * Whether this view is the currently-visible pane. Hidden views still
   * keep their Terminal attached so output keeps flowing into the
   * scrollback buffer, mirroring `TerminalView`'s contract.
   */
  isActive: boolean;
}

export function SubTerminalView({ subSessionId, isActive }: SubTerminalViewProps): JSX.Element {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sub = useSubSessionById(subSessionId);
  const { attach, detach, focus, refit } = useSubTerminal(subSessionId);

  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    attach(el);
    return () => {
      detach();
    };
  }, [attach, detach]);

  // Same rationale as TerminalView: visibility:hidden panels don't fire
  // ResizeObserver, so re-measure + steal focus on the activation edge.
  useEffect(() => {
    if (!isActive) return;
    const handle = requestAnimationFrame(() => {
      refit();
      focus();
    });
    return () => cancelAnimationFrame(handle);
  }, [isActive, refit, focus]);

  return (
    <div
      role="tabpanel"
      aria-label={sub ? `Terminal for ${sub.label}` : 'Sub-session terminal'}
      className="relative h-full w-full bg-black p-2"
    >
      <div ref={containerRef} className="h-full w-full bg-black" />
    </div>
  );
}
