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
  /** Set as the native `title` attribute for hover-tooltip + assistive-tech label. */
  title?: string;
}

// Codepoints lifted from the Nerd Fonts v3 `glyphnames.json` catalogue.
// Keep the comments in sync with the upstream `nf-cod-…` name so a
// future maintainer can re-derive the codepoint from the cheatsheet.
const STATUS_GLYPHS: Record<Exclude<DisplayStatus, 'idle'>, string> = {
  starting: '\ueb19', // nf-cod-loading — partial-ring spinner (pairs with animate-spin)
  working: '\uec10', // nf-cod-sparkle — "model is generating"
  awaiting: '\uea6b', // nf-cod-comment — speech bubble: agent is parked at its prompt
  attention: '\ueaa2', // nf-cod-bell — matches OSC 9 / OSC 777 / standalone BEL semantics
  awaitingPermission: '\uea75', // nf-cod-lock — agent is blocked on a permission decision
  runningTool: '\ueb6d', // nf-cod-tools — agent invoked a tool
  thinking: '\uea7c', // nf-cod-ellipsis — assistant turn in flight (pairs with animate-pulse)
  exited: '\uead7', // nf-cod-debug_stop — terminal-state filled square
  error: '\uea87', // nf-cod-error — circle with bang
};

// Stable kebab-case suffix for the test id. Predates the camelCase
// `awaitingPermission` / `runningTool` enum names — kept verbatim so any
// existing or external assertion against the previous testid keeps
// working after the SVG → glyph swap.
const STATUS_TESTID_SUFFIX: Record<Exclude<DisplayStatus, 'idle'>, string> = {
  starting: 'starting',
  working: 'working',
  awaiting: 'awaiting',
  attention: 'attention',
  awaitingPermission: 'awaiting-permission',
  runningTool: 'running-tool',
  thinking: 'thinking',
  exited: 'exited',
  error: 'error',
};

// `inline-block w-[1em] text-center` pins the glyph's box to a square
// the size of the current font, so layout stays stable when the active
// state — and therefore the codepoint — changes. `leading-none` strips
// the inherited line-box so the box's reported height equals its
// font-size; the unread-overlay dot in `SidebarTab` anchors off this
// box and would jitter otherwise. `font-sans` pins the glyph to the
// bundled `CaskaydiaCove NF` family even when the icon is rendered
// inside a `font-mono` ancestor (e.g. terminal-adjacent surfaces).
const BASE_CLASSES = 'inline-block w-[1em] text-center font-sans leading-none';

export function StatusIcon({ status, className, title }: StatusIconProps): JSX.Element | null {
  // `idle` intentionally renders nothing — a quiescent session shouldn't
  // shout at the user. Returning null keeps the icon column blank rather
  // than reserving space for an absent glyph.
  if (status === 'idle') return null;

  const glyph = STATUS_GLYPHS[status];
  const testId = `status-icon-${STATUS_TESTID_SUFFIX[status]}`;

  return (
    <span
      className={className ? `${BASE_CLASSES} ${className}` : BASE_CLASSES}
      // Provide an a11y label only when the caller supplied a meaningful
      // tooltip; otherwise hide from assistive tech entirely (the parent
      // tab already announces the session). Mixing aria-label with
      // aria-hidden=true is a contradiction screen readers will warn on.
      aria-label={title ?? undefined}
      aria-hidden={title ? undefined : true}
      role={title ? 'img' : undefined}
      title={title}
      data-testid={testId}
    >
      {glyph}
    </span>
  );
}
