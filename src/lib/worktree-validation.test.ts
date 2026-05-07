import { describe, expect, it } from 'vitest';
import { validateWorktreeName } from './worktree-validation';

describe('validateWorktreeName', () => {
  it.each(['feature-x', 'fix/123', 'release-1.2.3', 'a', 'user/topic-2024'])('accepts %j', (name) => {
    expect(validateWorktreeName(name)).toBeNull();
  });

  it.each<[string, string]>([
    ['', 'name cannot be empty'],
    ['has space', 'name cannot contain spaces'],
    ['..', "name cannot contain '..'"],
    ['foo..bar', "name cannot contain '..'"],
    ['@', "name cannot be '@'"],
    ['.hidden', "name cannot start with '.' or '/'"],
    ['/abs', "name cannot start with '.' or '/'"],
    ['trailing.', "name cannot end with '.' or '/'"],
    ['trailing/', "name cannot end with '.' or '/'"],
    ['name.lock', "name cannot end with '.lock'"],
    ['weird~name', "name cannot contain '~'"],
    ['weird^name', "name cannot contain '^'"],
    ['weird:name', "name cannot contain ':'"],
    ['weird?name', "name cannot contain '?'"],
    ['weird*name', "name cannot contain '*'"],
    ['weird[name', "name cannot contain '['"],
    ['weird\\name', "name cannot contain '\\'"],
    ['-bad', "name cannot start with '-'"],
    ['foo@{bar', "name cannot contain '@{'"],
    ['foo//bar', "name cannot contain '//'"],
    ['foo\tbar', 'name cannot contain control characters'],
    ['foo\nbar', 'name cannot contain control characters'],
    ['foo\x7fbar', 'name cannot contain control characters'],
    ['feature/.hidden', "name path components cannot start with '.'"],
    ['feature/foo.lock/bar', "name path components cannot end with '.lock'"],
  ])('rejects %j', (name, expected) => {
    expect(validateWorktreeName(name)).toBe(expected);
  });

  it('rejects names longer than 255 characters', () => {
    expect(validateWorktreeName('a'.repeat(256))).toBe('name cannot exceed 255 characters');
    expect(validateWorktreeName('a'.repeat(255))).toBeNull();
  });
});
