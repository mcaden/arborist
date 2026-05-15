import { describe, expect, it } from 'vitest';

import { isInsideWorktreesDir, pathsEqual } from './worktree-paths';

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

  it('handles filesystem root as workspace root without double slash', () => {
    expect(isInsideWorktreesDir('/', '/.arborist/.worktrees/foo')).toBe(true);
    expect(isInsideWorktreesDir('C:/', 'C:/.arborist/.worktrees/foo')).toBe(true);
    expect(isInsideWorktreesDir('////', '/.arborist/.worktrees/foo')).toBe(true);
  });
});

describe('pathsEqual', () => {
  it('matches identical posix paths', () => {
    expect(pathsEqual('/repo/.worktrees/foo', '/repo/.worktrees/foo')).toBe(true);
  });

  it('matches with mixed separators', () => {
    expect(pathsEqual(String.raw`C:\repos\arborist`, 'C:/repos/arborist')).toBe(true);
  });

  it('matches Windows paths case-insensitively', () => {
    expect(pathsEqual(String.raw`C:\Repos\Arborist`, 'c:/repos/arborist')).toBe(true);
  });

  it('compares posix paths case-sensitively', () => {
    expect(pathsEqual('/Repo/foo', '/repo/foo')).toBe(false);
  });

  it('ignores trailing slashes', () => {
    expect(pathsEqual('/repo/foo/', '/repo/foo')).toBe(true);
  });

  it('rejects different paths', () => {
    expect(pathsEqual('/repo/foo', '/repo/bar')).toBe(false);
  });

  it('handles UNC paths case-insensitively', () => {
    expect(pathsEqual(String.raw`\\Server\Share\project`, String.raw`\\server\share\project`)).toBe(true);
  });

  it('handles root paths correctly', () => {
    expect(pathsEqual('/', '/')).toBe(true);
    expect(pathsEqual('C:/', 'C:\\')).toBe(true);
    expect(pathsEqual('/', '/repo')).toBe(false);
    expect(pathsEqual('C:/', 'C:/repo')).toBe(false);
  });

  it('normalizes multiple trailing slashes to root', () => {
    expect(pathsEqual('////', '/')).toBe(true);
    expect(pathsEqual('C:////', 'C:/')).toBe(true);
    expect(pathsEqual('C:////', 'c:\\')).toBe(true);
  });
});
