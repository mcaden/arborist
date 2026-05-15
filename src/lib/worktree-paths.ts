// Cross-platform path utilities used by the worktree UI. Kept in a
// dedicated module so the helpers can be unit-tested without dragging in
// React.
//
// We deliberately do *not* try to be a full path library — that would
// pull in node-only deps. Instead we implement the narrow comparisons
// the UI needs, with explicit handling for both POSIX and Windows
// conventions.

/**
 * Heuristic: is the path a Windows-style absolute path (drive letter or
 * UNC)? Used to decide whether comparisons should be case-insensitive.
 * Pure POSIX paths are always compared case-sensitively.
 */
function isWindowsLikePath(p: string): boolean {
  return /^[A-Za-z]:[\\/]/.test(p) || /^[\\/]{2}/.test(p);
}

/** Normalize separators to `/` and strip trailing slashes (preserving root paths). */
function normalize(p: string): string {
  if (p.length === 0) return '';
  // Replacing backslashes globally is safe enough for our purpose:
  // backslashes in literal POSIX file names are rare, and the inputs to
  // this helper come from `git worktree list --porcelain` and our own
  // workspace-root config — neither produces such names in practice.
  const slashed = p.replace(/\\/g, '/');
  // Drive root with any number of trailing slashes (e.g. `C:/`, `C:////`) → `X:/`.
  if (/^[A-Za-z]:\/+$/.test(slashed)) return slashed[0] + ':/';
  // Strip trailing slashes without regex quantifiers (avoids SonarCloud ReDoS false positive).
  let end = slashed.length;
  while (end > 0 && slashed[end - 1] === '/') end--;
  // All slashes stripped → input was all slashes → POSIX root.
  if (end === 0) return '/';
  return slashed.slice(0, end);
}

/**
 * `true` iff `child` is *strictly* inside `<root>/.arborist/.worktrees/` —
 * i.e. `<root>/.arborist/.worktrees/<at least one component>`. Both `/`
 * and `\` separators are accepted on either side, and on Windows-style
 * paths the comparison is case-insensitive (to match Windows filesystem
 * semantics where `C:\Repo` and `c:\repo` refer to the same directory).
 *
 * The `.arborist/.worktrees` layout was introduced in issue #71; older
 * installations placing worktrees directly under `<root>/.worktrees/`
 * are not auto-discovered (hard cut-over per the issue acceptance).
 */
export function isInsideWorktreesDir(root: string, child: string): boolean {
  const r = normalize(root);
  const c = normalize(child);
  // Avoid double-slash when root is `/` or `C:/` (already ends with `/`).
  const prefix = r.endsWith('/') ? `${r}.arborist/.worktrees/` : `${r}/.arborist/.worktrees/`;
  const winLike = isWindowsLikePath(r) || isWindowsLikePath(c);
  if (winLike) {
    const cl = c.toLowerCase();
    const pl = prefix.toLowerCase();
    return cl.startsWith(pl) && cl.length > pl.length;
  }
  return c.startsWith(prefix) && c.length > prefix.length;
}

/**
 * Cross-platform path equality: normalizes separators, trims trailing
 * slashes, and case-folds on Windows-like paths.
 */
export function pathsEqual(a: string, b: string): boolean {
  const na = normalize(a);
  const nb = normalize(b);
  if (isWindowsLikePath(na) || isWindowsLikePath(nb)) {
    return na.toLowerCase() === nb.toLowerCase();
  }
  return na === nb;
}
