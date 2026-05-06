// Inline SVG status glyphs for the sidebar. Replaces the colored-dot
// indicators that lived in `SidebarTab` with one icon per
// [`DisplayStatus`]. Kept as inline SVGs (no icon-library dependency)
// to match the convention established by `ToolIcon.tsx`.
//
// Each icon paints from `currentColor` so Tailwind text-color classes
// recolour them; the per-state color is owned by the caller and is set
// via the `className` prop. Sizes are uniform (h-3.5 w-3.5 by default
// at the call site) so swapping states does not nudge layout.

import type { DisplayStatus } from '@/store/session-store';

interface StatusIconProps {
  status: DisplayStatus;
  className?: string;
  /** Set as `<title>` for hover-tooltip + assistive-tech label. */
  title?: string;
}

export function StatusIcon({ status, className, title }: StatusIconProps): JSX.Element | null {
  // `idle` intentionally renders nothing — a quiescent session shouldn't
  // shout at the user. Returning null keeps the icon column blank rather
  // than reserving space for an absent glyph.
  if (status === 'idle') return null;

  const common = {
    xmlns: 'http://www.w3.org/2000/svg',
    viewBox: '0 0 24 24',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 2,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
    // Provide an a11y label only when the caller supplied a meaningful
    // tooltip; otherwise hide from assistive tech entirely (the parent
    // tab already announces the session). Mixing aria-label with
    // aria-hidden=true is a contradiction screen readers will warn on.
    'aria-label': title ?? undefined,
    'aria-hidden': title ? undefined : true,
    role: title ? ('img' as const) : undefined,
    className,
  };

  switch (status) {
    case 'starting':
      // Three-quarter ring with the gap pointing up-right; pairs with
      // an `animate-spin` class supplied by the caller for the spinner
      // effect.
      return (
        <svg {...common} data-testid="status-icon-starting">
          {title ? <title>{title}</title> : null}
          <path d="M12 3 a9 9 0 1 1 -9 9" />
        </svg>
      );

    case 'working':
      // Sparkles — evokes "the model is generating".
      return (
        <svg {...common} data-testid="status-icon-working">
          {title ? <title>{title}</title> : null}
          <path d="M12 3 l1.6 4.4 L18 9 l-4.4 1.6 L12 15 l-1.6-4.4 L6 9 l4.4-1.6 z" />
          <path d="M18 14 l0.8 2.2 L21 17 l-2.2 0.8 L18 20 l-0.8-2.2 L15 17 l2.2-0.8 z" />
        </svg>
      );

    case 'awaiting':
      // Speech bubble — "the agent has finished and is waiting for you
      // to say something".
      return (
        <svg {...common} data-testid="status-icon-awaiting">
          {title ? <title>{title}</title> : null}
          <path d="M4 5 h16 a1 1 0 0 1 1 1 v10 a1 1 0 0 1 -1 1 h-9 l-4 3 v-3 H4 a1 1 0 0 1 -1 -1 V6 a1 1 0 0 1 1 -1 z" />
        </svg>
      );

    case 'attention':
      // Bell — matches OSC 9 / OSC 777 / standalone BEL semantics.
      return (
        <svg {...common} data-testid="status-icon-attention">
          {title ? <title>{title}</title> : null}
          <path d="M6 16 V11 a6 6 0 0 1 12 0 v5 l1.5 2 H4.5 z" />
          <path d="M10 20 a2 2 0 0 0 4 0" />
        </svg>
      );

    case 'awaitingPermission':
      // Padlock — agent is blocked on a permission decision. The amber
      // colour (set by the caller via className) deliberately matches
      // `attention` so the user reads "this tab needs me" at a glance.
      return (
        <svg {...common} data-testid="status-icon-awaiting-permission">
          {title ? <title>{title}</title> : null}
          <rect x="5" y="11" width="14" height="9" rx="2" />
          <path d="M8 11 V8 a4 4 0 0 1 8 0 v3" />
        </svg>
      );

    case 'runningTool':
      // Wrench — distinct from the sparkle (`working`) and dots
      // (`thinking`) so a glance at the icon answers "what is the agent
      // doing right now?".
      return (
        <svg {...common} data-testid="status-icon-running-tool">
          {title ? <title>{title}</title> : null}
          <path d="M14.7 3.5 a4.5 4.5 0 0 0 5.8 5.8 L13 16.8 l-2.8-2.8 z" />
          <line x1="9.5" y1="14.5" x2="4.5" y2="19.5" />
        </svg>
      );

    case 'thinking':
      // Three dots — universal "agent is processing". Pairs with an
      // `animate-pulse` class supplied by the caller.
      return (
        <svg {...common} data-testid="status-icon-thinking" fill="currentColor" stroke="none">
          {title ? <title>{title}</title> : null}
          <circle cx="6" cy="12" r="1.5" />
          <circle cx="12" cy="12" r="1.5" />
          <circle cx="18" cy="12" r="1.5" />
        </svg>
      );

    case 'exited':
      // Filled stop square — terminal-state indicator.
      return (
        <svg {...common} fill="currentColor" data-testid="status-icon-exited">
          {title ? <title>{title}</title> : null}
          <rect x="6" y="6" width="12" height="12" rx="1.5" />
        </svg>
      );

    case 'error':
      // Triangle with a bang — universal "something went wrong".
      return (
        <svg {...common} data-testid="status-icon-error">
          {title ? <title>{title}</title> : null}
          <path d="M12 3 L22 20 H2 z" />
          <line x1="12" y1="10" x2="12" y2="14" />
          <line x1="12" y1="17" x2="12" y2="17.01" />
        </svg>
      );

    default: {
      // Exhaustive: TS will flag a new DisplayStatus variant here.
      const _exhaustive: never = status;
      void _exhaustive;
      return null;
    }
  }
}
