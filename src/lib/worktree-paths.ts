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

/** Normalize separators to `/` and strip trailing slashes. */
function normalize(p: string): string {
  // Replacing backslashes globally is safe enough for our purpose:
  // backslashes in literal POSIX file names are rare, and the inputs to
  // this helper come from `git worktree list --porcelain` and our own
  // workspace-root config — neither produces such names in practice.
  return p.replace(/\\/g, '/').replace(/\/+$/, '');
}

/**
 * `true` iff `value` looks like an absolute path on either POSIX (leading `/`)
 * or Windows (drive letter or UNC). Used to decide whether a configured
 * `worktreesDir` should be resolved against the workspace root or used as-is.
 */
function isAbsolutePath(value: string): boolean {
  return value.startsWith('/') || isWindowsLikePath(value);
}

/**
 * `true` iff `value` is "rooted but has no prefix" *and* `repoRoot` is
 * Windows-like — e.g. `\worktrees` or `/worktrees` against `C:\repo` or
 * `\\srv\share\repo`. On Windows such paths are absolute on the same drive
 * (or UNC share) as the cwd, and Rust's `PathBuf::join` preserves the
 * workspace's prefix while replacing the rest. We mirror that behaviour
 * here so the frontend filter agrees with backend `compose::resolve_worktrees_dir`.
 *
 * The check rejects (a) values that are already fully absolute (UNC `\\srv\…`
 * or drive `C:\…`), and (b) any value when `repoRoot` is POSIX — on POSIX,
 * `\foo` is just a strange relative path and should be joined like any other.
 */
function isWindowsRootedNoPrefix(value: string, repoRoot: string): boolean {
  if (!isWindowsLikePath(repoRoot)) return false;
  if (!/^[\\/]/.test(value)) return false;
  if (/^[\\/]{2}/.test(value)) return false;
  if (/^[A-Za-z]:[\\/]/.test(value)) return false;
  return true;
}

/**
 * Extract the Windows-style filesystem prefix from `repoRoot` — the drive
 * letter (`C:`) or the UNC `//server/share` portion. Returns the empty string
 * for POSIX inputs or anything we cannot recognise, which is fine because the
 * caller only uses this when `isWindowsRootedNoPrefix` already returned true.
 */
function windowsPrefix(repoRoot: string): string {
  const normRoot = repoRoot.replace(/\\/g, '/');
  const uncMatch = /^(\/\/[^/]+\/[^/]+)/.exec(normRoot);
  if (uncMatch) return uncMatch[1]!;
  const driveMatch = /^([A-Za-z]:)/.exec(normRoot);
  if (driveMatch) return driveMatch[1]!;
  return '';
}

/**
 * Walk `.` and `..` segments without touching the filesystem. Mirrors the
 * Rust-side `compose::lexical_normalise` so the frontend warning, the live
 * `worktrees_dir_check` preview, and `worktree_create_impl` agree on whether
 * a relative `..`-laden value escapes the workspace.
 *
 * Pure: absolute → leading separator preserved, relative → kept relative.
 * Windows-style drive prefixes (`C:`) are passed through verbatim.
 */
function lexicalResolve(parts: string[]): string {
  const out: string[] = [];
  for (const seg of parts) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') {
      // Pop only when the previous element is a regular name. Preserve `..`
      // against a leading `..` chain or a root-anchor (handled by the caller
      // re-prefixing the leading separator).
      if (out.length > 0 && out[out.length - 1] !== '..') {
        out.pop();
      } else {
        out.push('..');
      }
      continue;
    }
    out.push(seg);
  }
  return out.join('/');
}

/**
 * Resolve a configured `worktreesDir` value against the workspace root and
 * return a normalised absolute path with `..` segments walked. Empty or
 * whitespace-only input collapses to the runtime default (`.worktrees`) so
 * the answer agrees with the backend's `merge_partial` normalisation.
 *
 * - Absolute `worktreesDir` is taken verbatim.
 * - Relative `worktreesDir` is joined onto `repoRoot` and then lexically
 *   resolved (so `..` may legitimately escape the workspace).
 *
 * Returns the absolute path with separators normalised to forward slashes
 * regardless of input style (Windows-style `D:\foo` becomes `D:/foo`). All
 * downstream string comparisons go through `normalize()`, which does the same
 * normalisation on the child path, so the styles always agree.
 *
 * The trailing `/` on `repoRoot`, if any, is stripped before joining.
 */
export function resolveWorktreesRoot(repoRoot: string, worktreesDir: string): string {
  const trimmed = worktreesDir.trim();
  const effective = trimmed === '' ? '.worktrees' : trimmed;
  // Order matters: Windows "rooted but no prefix" (`\foo`, `/foo` against a Windows workspace) MUST be handled before the generic absolute-path
  // branch — otherwise leading `/` would short-circuit to "POSIX absolute" and we'd resolve to `/foo` instead of mirroring Rust's `C:\repo`.join(`/foo`)
  // = `C:\foo`. PR #70 review.
  if (isWindowsRootedNoPrefix(effective, repoRoot)) {
    const prefix = windowsPrefix(repoRoot);
    const valueNorm = effective.replace(/\\/g, '/');
    return joinAndResolve(`${prefix}${valueNorm}`, []);
  }
  if (isAbsolutePath(effective)) {
    // Absolute → still walk `.`/`..` so a literal `/var/wt/.` collapses.
    return joinAndResolve(effective, []);
  }
  return joinAndResolve(repoRoot, effective.replace(/\\/g, '/').split('/'));
}

/**
 * Combine an absolute `root` with extra relative segments, then lexically
 * resolve the whole path. Preserves the leading POSIX separator and Windows
 * drive/UNC prefix on the result.
 */
function joinAndResolve(root: string, extra: string[]): string {
  const normRoot = root.replace(/\\/g, '/');
  let prefix = '';
  let body = normRoot;
  // Match leading anchors so they survive `..` walking. Order matters: UNC
  // is two slashes plus a server name, drive letter is `X:/`, plain POSIX is
  // a single leading `/`.
  const uncMatch = /^(\/\/[^/]+\/[^/]+)(\/.*)?$/.exec(normRoot);
  const driveMatch = /^([A-Za-z]:)(\/.*)?$/.exec(normRoot);
  if (uncMatch) {
    prefix = uncMatch[1]!;
    body = uncMatch[2] ?? '';
  } else if (driveMatch) {
    prefix = driveMatch[1]!;
    body = driveMatch[2] ?? '';
  } else if (normRoot.startsWith('/')) {
    prefix = '';
    body = normRoot;
  }
  const isAnchored = prefix !== '' || body.startsWith('/');
  const segments = body.split('/').concat(extra);
  const resolved = lexicalResolve(segments);
  if (isAnchored) {
    return resolved === '' ? `${prefix}/` : `${prefix}/${resolved}`;
  }
  return resolved;
}

/**
 * `true` iff `child` is *strictly* inside the configured worktrees folder
 * derived from `(repoRoot, worktreesDir)`. Both `/` and `\` separators are
 * accepted on either side, and on Windows-style paths the comparison is
 * case-insensitive (to match Windows filesystem semantics where `C:\Repo`
 * and `c:\repo` refer to the same directory).
 *
 * Returns `false` when the resolved worktrees root is the child path itself
 * (i.e. requires at least one extra path component below it).
 */
export function isInsideWorktreesDir(repoRoot: string, worktreesDir: string, child: string): boolean {
  const root = resolveWorktreesRoot(repoRoot, worktreesDir);
  const c = normalize(child);
  // `root` may already end with `/` for filesystem roots (POSIX `/`, drive `C:/`, UNC `//srv/share/`)
  // when the configured worktreesDir collapses to an anchor (e.g. `..` against `/repo`). Doubling the
  // slash here would break the prefix check for valid children like `/foo`.
  const prefix = root.endsWith('/') ? root : `${root}/`;
  const winLike = isWindowsLikePath(repoRoot) || isWindowsLikePath(child) || isWindowsLikePath(root);
  if (winLike) {
    const cl = c.toLowerCase();
    const pl = prefix.toLowerCase();
    return cl.startsWith(pl) && cl.length > pl.length;
  }
  return c.startsWith(prefix) && c.length > prefix.length;
}
