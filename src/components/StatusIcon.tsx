// Status indicator glyphs for the sidebar — one Nerd Font codicon per
// [`DisplayStatus`] so the right-hand status column reads as fast as a
// row of text. Replaces the inline SVGs that lived here previously,
// piggy-backing on the bundled `CaskaydiaCove NF` font (added in #61)
// so we don't ship a second icon library.
//
// Codicons live in the U+EA60–U+EC1E PUA range and are guaranteed to be
// present in any Nerd Font v3.x build. See
// https://www.nerdfonts.com/cheat-sheet for the upstream catalogue;
// each glyph below cites the upstream `nf-cod-…` name as a lookup key
// for future swaps.
//
// The component renders a plain `<span>`. The caller owns sizing
// (`text-*`), colour (`text-{colour}-{shade}`) and animation
// (`animate-{spin,pulse}`) classes via the `className` prop. We pin the
// glyph's box to `1em × 1em` so the surrounding layout doesn't shift
// when the state — and therefore the glyph — changes.

import type { DisplayStatus } from '@/store/session-store';

interface StatusIconProps {
  status: DisplayStatus;
  className?: string;
  /** Set as the native `title` attribute for hover-tooltip + assistive-tech label. Empty / whitespace-only strings are treated as absent. */
  title?: string;
}

interface StatusMeta {
  /** Single-codepoint Nerd Font codicon glyph (U+EA60–U+EC1E PUA range). */
  glyph: string;
  /** Stable kebab-case suffix appended to `status-icon-` for the test id. */
  testIdSuffix: string;
}

// Single source of truth for per-status presentation. Co-locating the
// glyph and the test-id suffix in one map prevents the two from drifting
// when a future `DisplayStatus` variant is added or renamed (TypeScript
// catches missing keys, but only when the maps stay typed against the
// same union — having one map removes the parallel-update footgun).
//
// `satisfies` keeps the per-key literal types narrow (so callers can
// rely on `STATUS_META.starting.glyph` being the actual codepoint
// string, not just `string`) while still enforcing exhaustiveness.
//
// The kebab-case test-id suffixes for the camelCase `awaitingPermission`
// and `runningTool` cases predate this swap; preserved verbatim so any
// existing assertion against the previous testid keeps working.
const STATUS_META = {
  starting: { glyph: '\ueb19', testIdSuffix: 'starting' }, // nf-cod-loading — partial-ring spinner (pairs with animate-spin)
  working: { glyph: '\uec10', testIdSuffix: 'working' }, // nf-cod-sparkle — "model is generating"
  awaiting: { glyph: '\uea6b', testIdSuffix: 'awaiting' }, // nf-cod-comment — speech bubble: agent is parked at its prompt
  attention: { glyph: '\ueaa2', testIdSuffix: 'attention' }, // nf-cod-bell — matches OSC 9 / OSC 777;notify semantics (standalone BEL ignored)
  awaitingPermission: { glyph: '\uea75', testIdSuffix: 'awaiting-permission' }, // nf-cod-lock — agent is blocked on a permission decision
  runningTool: { glyph: '\ueb6d', testIdSuffix: 'running-tool' }, // nf-cod-tools — agent invoked a tool
  thinking: { glyph: '\uea7c', testIdSuffix: 'thinking' }, // nf-cod-ellipsis — assistant turn in flight (pairs with animate-pulse)
  exited: { glyph: '\uead7', testIdSuffix: 'exited' }, // nf-cod-debug_stop — terminal-state filled square
  error: { glyph: '\uea87', testIdSuffix: 'error' }, // nf-cod-error — circle with bang
} as const satisfies Record<Exclude<DisplayStatus, 'idle'>, StatusMeta>;

// `inline-block w-[1em] text-center` pins the glyph's box to a square
// the size of the current font, so layout stays stable when the active
// state — and therefore the codepoint — changes. `leading-none` strips
// the inherited line-box so the box's reported height equals its
// font-size; the unread-overlay dot in `SidebarTab` anchors off this
// box and would jitter otherwise. `font-icon` is a dedicated Tailwind
// family that contains only `'CaskaydiaCove NF'` — explicitly decoupled
// from the `sans` stack so reorganising the body font can't break icon
// rendering, and intentionally without a generic fallback so a missing
// glyph renders as tofu instead of silently picking up a system-font
// substitute that lacks the codicon PUA codepoints.
const BASE_CLASSES = 'inline-block w-[1em] text-center font-icon leading-none';

export function StatusIcon({ status, className, title }: StatusIconProps): JSX.Element | null {
  // `idle` intentionally renders nothing — a quiescent session shouldn't
  // shout at the user. Returning null keeps the icon column blank rather
  // than reserving space for an absent glyph.
  if (status === 'idle') return null;

  const { glyph, testIdSuffix } = STATUS_META[status];
  const testId = `status-icon-${testIdSuffix}`;

  // Treat empty- and whitespace-only `title` strings as "no title"
  // rather than as a meaningful tooltip. Without this normalisation,
  // `title=""` produces a contradictory pair of a11y signals — `??`
  // preserves the empty string for `aria-label`, while `title ? …`
  // treats it as falsy and still emits `aria-hidden="true"`. Trimming
  // also rules out whitespace-only labels, which are never a useful
  // tooltip in practice.
  const trimmedTitle = title?.trim();
  const hasTitle = trimmedTitle !== undefined && trimmedTitle.length > 0;
  const labelTitle = hasTitle ? trimmedTitle : undefined;

  return (
    <span
      className={className ? `${BASE_CLASSES} ${className}` : BASE_CLASSES}
      // Provide an a11y label only when the caller supplied a meaningful
      // tooltip; otherwise hide from assistive tech entirely (the parent
      // tab already announces the session). Mixing aria-label with
      // aria-hidden=true is a contradiction screen readers will warn on,
      // so route everything through the same `hasTitle` boolean.
      aria-label={labelTitle}
      aria-hidden={hasTitle ? undefined : true}
      role={hasTitle ? 'img' : undefined}
      title={labelTitle}
      data-testid={testId}
    >
      {glyph}
    </span>
  );
}
