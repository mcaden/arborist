import { describe, expect, it } from 'vitest';

import { isInsideWorktreesDir, resolveWorktreesRoot } from './worktree-paths';

describe('isInsideWorktreesDir', () => {
  it('accepts a posix child inside the default .worktrees', () => {
    expect(isInsideWorktreesDir('/repo', '.worktrees', '/repo/.worktrees/foo')).toBe(true);
  });

  it('accepts trailing slash on root', () => {
    expect(isInsideWorktreesDir('/repo/', '.worktrees', '/repo/.worktrees/foo')).toBe(true);
  });

  it('rejects the main checkout (root itself)', () => {
    expect(isInsideWorktreesDir('/repo', '.worktrees', '/repo')).toBe(false);
  });

  it('rejects the .worktrees directory with no child component', () => {
    expect(isInsideWorktreesDir('/repo', '.worktrees', '/repo/.worktrees')).toBe(false);
    expect(isInsideWorktreesDir('/repo', '.worktrees', '/repo/.worktrees/')).toBe(false);
  });

  it('rejects a sibling that merely starts with the same prefix', () => {
    expect(isInsideWorktreesDir('/repo', '.worktrees', '/repo/.worktrees-foo/bar')).toBe(false);
  });

  it('accepts mixed separators', () => {
    expect(isInsideWorktreesDir('C:\\Repo', '.worktrees', 'C:\\Repo\\.worktrees\\foo')).toBe(true);
    expect(isInsideWorktreesDir('C:\\Repo', '.worktrees', 'C:/Repo/.worktrees/foo')).toBe(true);
  });

  it('compares Windows-style paths case-insensitively', () => {
    expect(isInsideWorktreesDir('C:\\Repo', '.worktrees', 'c:\\repo\\.worktrees\\foo')).toBe(true);
    expect(isInsideWorktreesDir('C:/Repo', '.worktrees', 'c:/repo/.worktrees/Foo')).toBe(true);
  });

  it('compares posix paths case-sensitively', () => {
    expect(isInsideWorktreesDir('/Repo', '.worktrees', '/repo/.worktrees/foo')).toBe(false);
  });

  it('handles UNC paths case-insensitively', () => {
    expect(isInsideWorktreesDir('\\\\server\\Share', '.worktrees', '\\\\Server\\share\\.worktrees\\foo')).toBe(true);
  });

  it('honours a custom relative worktreesDir', () => {
    expect(isInsideWorktreesDir('/repo', 'wt', '/repo/wt/foo')).toBe(true);
    expect(isInsideWorktreesDir('/repo', 'wt', '/repo/.worktrees/foo')).toBe(false);
  });

  it('honours an absolute worktreesDir outside the workspace', () => {
    expect(isInsideWorktreesDir('/repo', '/var/wt', '/var/wt/foo')).toBe(true);
    expect(isInsideWorktreesDir('/repo', '/var/wt', '/repo/.worktrees/foo')).toBe(false);
  });

  it('honours a relative worktreesDir that escapes the workspace', () => {
    expect(isInsideWorktreesDir('/repo/sub', '../wt', '/repo/wt/foo')).toBe(true);
  });

  it('treats empty worktreesDir as the .worktrees default', () => {
    expect(isInsideWorktreesDir('/repo', '', '/repo/.worktrees/foo')).toBe(true);
    expect(isInsideWorktreesDir('/repo', '   ', '/repo/.worktrees/foo')).toBe(true);
  });

  // Regression for PR #70 review: when worktreesDir resolves to a filesystem
  // root (POSIX `/`, drive `C:/`, UNC `//srv/share/`), the prefix check used
  // to double the trailing slash and incorrectly reject every child.
  it('handles a worktrees root that already ends with a separator (POSIX /)', () => {
    expect(isInsideWorktreesDir('/repo', '..', '/foo')).toBe(true);
    expect(isInsideWorktreesDir('/repo', '..', '/')).toBe(false);
  });

  it('handles a worktrees root that collapses to a Windows drive root', () => {
    expect(isInsideWorktreesDir('C:/Repo', '..', 'C:/foo')).toBe(true);
    expect(isInsideWorktreesDir('C:/Repo', '..', 'C:/')).toBe(false);
  });

  it('handles a worktrees root that collapses to a UNC share root', () => {
    expect(isInsideWorktreesDir('//srv/share/repo', '..', '//srv/share/foo')).toBe(true);
    expect(isInsideWorktreesDir('//srv/share/repo', '..', '//srv/share/')).toBe(false);
  });

  // PR #70 review: Windows "rooted but no prefix" paths (`\foo`, `/foo`) are
  // absolute on the workspace's drive in Rust's `PathBuf::join` semantics, so
  // the frontend filter must agree — otherwise NewSessionDialog would show a
  // worktree as "existing under .worktrees/" when git actually placed it at
  // the drive root.
  it('treats Windows rooted-no-prefix worktreesDir as drive-root absolute', () => {
    expect(isInsideWorktreesDir('C:\\Repo', '\\foo', 'C:/foo/bar')).toBe(true);
    expect(isInsideWorktreesDir('C:\\Repo', '\\foo', 'C:/Repo/.worktrees/bar')).toBe(false);
    expect(isInsideWorktreesDir('C:/Repo', '/foo', 'C:/foo/bar')).toBe(true);
  });

  it('treats UNC rooted-no-prefix worktreesDir as share-root absolute', () => {
    expect(isInsideWorktreesDir('\\\\srv\\share\\repo', '\\wt', '//srv/share/wt/foo')).toBe(true);
    expect(isInsideWorktreesDir('\\\\srv\\share\\repo', '\\wt', '//srv/share/repo/.worktrees/foo')).toBe(false);
  });

  it('does NOT apply Windows-rooted treatment when repoRoot is POSIX', () => {
    // On POSIX, a literal-backslash `\foo` is just an unusual relative path; we join it under
    // the workspace root with the rest of the normalisation pipeline (which collapses `\` to `/`).
    // The point of this test is that we do NOT try to extract a drive prefix from a POSIX root.
    expect(resolveWorktreesRoot('/repo', '\\foo')).toBe('/repo/foo');
  });
});

describe('resolveWorktreesRoot', () => {
  it('joins a relative dir against the workspace root', () => {
    expect(resolveWorktreesRoot('/repo', '.worktrees')).toBe('/repo/.worktrees');
    expect(resolveWorktreesRoot('/repo', 'wt')).toBe('/repo/wt');
  });

  it('returns an absolute dir verbatim (POSIX)', () => {
    expect(resolveWorktreesRoot('/repo', '/var/wt')).toBe('/var/wt');
  });

  it('returns an absolute Windows dir verbatim', () => {
    expect(resolveWorktreesRoot('C:\\Repo', 'D:\\elsewhere')).toBe('D:/elsewhere');
  });

  it('walks `..` segments lexically', () => {
    expect(resolveWorktreesRoot('/repo/sub', '../wt')).toBe('/repo/wt');
  });

  it('collapses leading whitespace / empty input to the default', () => {
    expect(resolveWorktreesRoot('/repo', '')).toBe('/repo/.worktrees');
    expect(resolveWorktreesRoot('/repo', '   ')).toBe('/repo/.worktrees');
  });
});
