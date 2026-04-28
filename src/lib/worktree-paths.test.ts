import { describe, expect, it } from 'vitest';

import { isInsideWorktreesDir } from './worktree-paths';

describe('isInsideWorktreesDir', () => {
  it('accepts a posix child inside .worktrees', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.worktrees/foo')).toBe(true);
  });

  it('accepts trailing slash on root', () => {
    expect(isInsideWorktreesDir('/repo/', '/repo/.worktrees/foo')).toBe(true);
  });

  it('rejects the main checkout (root itself)', () => {
    expect(isInsideWorktreesDir('/repo', '/repo')).toBe(false);
  });

  it('rejects the .worktrees directory with no child component', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.worktrees')).toBe(false);
    expect(isInsideWorktreesDir('/repo', '/repo/.worktrees/')).toBe(false);
  });

  it('rejects a sibling that merely starts with the same prefix', () => {
    expect(isInsideWorktreesDir('/repo', '/repo/.worktrees-foo/bar')).toBe(false);
  });

  it('accepts mixed separators', () => {
    expect(isInsideWorktreesDir('C:\\Repo', 'C:\\Repo\\.worktrees\\foo')).toBe(true);
    expect(isInsideWorktreesDir('C:\\Repo', 'C:/Repo/.worktrees/foo')).toBe(true);
  });

  it('compares Windows-style paths case-insensitively', () => {
    expect(isInsideWorktreesDir('C:\\Repo', 'c:\\repo\\.worktrees\\foo')).toBe(true);
    expect(isInsideWorktreesDir('C:/Repo', 'c:/repo/.worktrees/Foo')).toBe(true);
  });

  it('compares posix paths case-sensitively', () => {
    expect(isInsideWorktreesDir('/Repo', '/repo/.worktrees/foo')).toBe(false);
  });

  it('handles UNC paths case-insensitively', () => {
    expect(isInsideWorktreesDir('\\\\server\\Share', '\\\\Server\\share\\.worktrees\\foo')).toBe(
      true,
    );
  });
});
