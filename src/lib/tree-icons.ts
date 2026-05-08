// Tree-icon URL resolver (Issue #45).
//
// Each WorktreeTab carries an `iconId: number` in `1..=WORKTREE_ICON_COUNT`
// that the backend assigns when the tab is first created. This module maps
// that integer to the bundled PNG asset Vite ships in `dist/assets/`.
//
// We use Vite's `import.meta.glob('…', { eager: true, query: '?url' })` so
// every tree icon is statically discoverable at build time and bundled
// together — adding a `tree_17.png` and bumping `WORKTREE_ICON_COUNT` on
// the Rust side is all that's required to extend the set.
//
// Runtime contract: callers pass the integer they got from a `WorktreeTab`
// and expect a non-empty string URL back. Out-of-range values (0, > N,
// negative, NaN) fall back to `tree_1.png` and emit at most one
// `console.warn` per unique invalid id for the lifetime of the page — so a
// regression in the backend assignment surfaces in dev exactly once per bad
// id rather than spamming the console on every re-render of the sidebar.

const ICON_MODULES = import.meta.glob<string>('../assets/tree-icons/tree_*.png', {
  eager: true,
  query: '?url',
  import: 'default',
});

// Build a 1-indexed map: id → URL. Vite returns paths with the leading `..`
// resolved relative to this file, so the keys look like
// `../assets/tree-icons/tree_3.png`. Parse the trailing integer and store.
const ICONS_BY_ID = new Map<number, string>();
for (const [path, url] of Object.entries(ICON_MODULES)) {
  const m = /tree_(\d+)\.png$/.exec(path);
  if (!m) continue;
  // The capture group is non-empty by construction (`\d+` matched), so
  // parseInt is safe — but guard NaN anyway in case the regex ever changes.
  const id = Number.parseInt(m[1] ?? '', 10);
  if (Number.isFinite(id) && id > 0) {
    ICONS_BY_ID.set(id, url);
  }
}

// Tracks which invalid `iconId`s we've already warned about so re-renders
// (or repeat lookups in a render-heavy view) don't flood the console.
// Module-scoped on purpose: the goal is one warning per unique bad id for
// the lifetime of the page, not per call site or per component instance.
const WARNED_IDS = new Set<number>();

/**
 * Number of bundled tree icons. Mirrors the Rust constant
 * `crate::worktree_icon::WORKTREE_ICON_COUNT`. Derived from the actual
 * imported set so a missing file (or a typo'd glob) shows up as a smaller
 * count rather than a runtime mismatch.
 */
export const WORKTREE_ICON_COUNT = ICONS_BY_ID.size;

/**
 * Resolve a `WorktreeTab.iconId` to a bundled asset URL. Returns the
 * `tree_1.png` URL as a fallback for ids outside `1..=WORKTREE_ICON_COUNT`
 * — the backend should never produce one, but the sidebar must still
 * render an `<img>` with a real `src` if it does. Emits at most one
 * `console.warn` per unique invalid id for the lifetime of the module
 * (re-renders with the same bad id stay silent).
 */
export function getTreeIconUrl(iconId: number): string {
  const direct = ICONS_BY_ID.get(iconId);
  if (direct) return direct;
  // Fallback path. `tree_1.png` is guaranteed to exist by the migration —
  // every workspace with at least one tab assigns icon_id >= 1.
  const fallback = ICONS_BY_ID.get(1);
  if (!fallback) {
    // Truly catastrophic — would only happen if the assets directory is
    // empty at build time. Returning an empty string surfaces a broken
    // image rather than crashing React.
    console.error('[tree-icons] no tree icons bundled — check src/assets/tree-icons/');
    return '';
  }
  // `Set<number>` uses SameValueZero equality, so `NaN` is correctly deduped (`new Set([NaN, NaN]).size === 1`) — no special handling needed for
  // out-of-range floats.
  if (!WARNED_IDS.has(iconId)) {
    WARNED_IDS.add(iconId);
    console.warn(`[tree-icons] iconId ${iconId} out of range (1..=${WORKTREE_ICON_COUNT}); falling back to tree_1.png`);
  }
  return fallback;
}
