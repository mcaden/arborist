// See `session.ts` for why this fixture is `.ts` rather than `.json`
// (TypeScript widens JSON-import literal types, defeating `satisfies`
// against `SessionView`'s tagged-union `tool` / `status` fields).

import type { SessionView } from '../arborist';

export const sessionViewFixture = {
  id: '550e8400-e29b-41d4-a716-446655440000',
  tool: 'claude',
  worktreePath: '/repo/feature-x',
  worktreeName: 'feature-x',
  label: 'feature-x',
  status: 'running',
  pid: 12345,
  createdAt: 1700000000,
  tabIndex: 0,
} as const satisfies SessionView;
