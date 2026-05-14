// Cross-boundary contract fixture for `Session`. Authored as a
// literally-typed TypeScript const (not raw JSON) so `satisfies
// Session` actually catches discriminated-union drift at compile time
// — TypeScript widens JSON-import literals (`tool: "claude"` →
// `tool: string`) which makes `satisfies` against tagged unions
// always fail spuriously, forcing an `as`-cast escape hatch that
// loses the drift check the test exists to enforce. Keeping the
// fixture in `.ts` with `as const satisfies T` preserves literals
// and gives us:
//   * compile-time mirror drift detection (rename / removal / type
//     change of any field in `Session` → `satisfies` fails),
//   * runtime key-set assertions in `arborist.test.ts` (catches
//     extra/missing keys vs. the declared TS interface).

import type { Session } from '../arborist';

export const sessionFixture = {
  id: '550e8400-e29b-41d4-a716-446655440000',
  tool: 'claude',
  worktreePath: '/repo/feature-x',
  worktreeName: 'feature-x',
  label: 'feature-x',
  composedCommand: 'claude --system-prompt /tmp/arborist/abc/sp.md',
  status: 'running',
  pid: 12345,
  createdAt: 1700000000,
  tabIndex: 0,
  tempFiles: [{ path: '/tmp/arborist/abc/sp.md', contents: 'context' }],
} as const satisfies Session;
