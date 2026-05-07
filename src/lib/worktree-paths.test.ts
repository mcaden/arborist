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
