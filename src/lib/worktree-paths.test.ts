import { describe, expect, it } from 'vitest';

import { isInsideWorktreesDir } from './worktree-paths';

// Path layout per issue #71: linked worktrees live under
// `<root>/.arborist/.worktrees/<name>` rather than the legacy
// `<root>/.worktrees/<name>` location. The legacy layout is *not*
// recognised — the cut-over is hard.

describe('isInsideWorktreesDir', () => {
  it('accepts a posix child inside .arborist/.worktrees', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.arborist/.worktrees/foo')).toBe(true);
  });

  it('accepts trailing slash on root', () => {
    expect(isInsideWorktreesDir('/repo/', '/repo/.arborist/.worktrees/foo')).toBe(true);
  });

  it('rejects the main checkout (root itself)', () => {
    expect(isInsideWorktreesDir('/repo', '/repo')).toBe(false);
  });

  it('rejects the .arborist/.worktrees directory with no child component', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.arborist/.worktrees')).toBe(false);
    expect(isInsideWorktreesDir('/repo', '/repo/.arborist/.worktrees/')).toBe(false);
  });

  it('rejects a sibling that merely starts with the same prefix', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.arborist/.worktrees-foo/bar')).toBe(false);
  });

  it('rejects the legacy <root>/.worktrees/ layout (hard cut-over per issue #71)', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.worktrees/foo')).toBe(false);
  });

  it('accepts mixed separators', () => {
    expect(isInsideWorktreesDir('C:\\Repo', 'C:\\Repo\\.arborist\\.worktrees\\foo')).toBe(true);
    expect(isInsideWorktreesDir('C:\\Repo', 'C:/Repo/.arborist/.worktrees/foo')).toBe(true);
  });

  it('compares Windows-style paths case-insensitively', () => {
    expect(isInsideWorktreesDir('C:\\Repo', 'c:\\repo\\.arborist\\.worktrees\\foo')).toBe(true);
    expect(isInsideWorktreesDir('C:/Repo', 'c:/repo/.arborist/.worktrees/Foo')).toBe(true);
  });

  it('compares posix paths case-sensitively', () => {
    expect(isInsideWorktreesDir('/Repo', '/repo/.arborist/.worktrees/foo')).toBe(false);
  });

  it('handles UNC paths case-insensitively', () => {
    expect(isInsideWorktreesDir('\\\\server\\Share', '\\\\Server\\share\\.arborist\\.worktrees\\foo')).toBe(true);
  });
});
