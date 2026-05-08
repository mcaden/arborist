// `.ts` rather than `.json` so the `kind` and `status` discriminators
// keep their literal types.

import type { SubSession } from '../arborist';

export const subSessionFixture = {
  id: '11111111-1111-1111-1111-111111111111',
  parentWorktreeTabId: '550e8400-e29b-41d4-a716-446655440000',
  defId: 'shell',
  kind: 'terminal',
  label: 'Shell',
  status: 'running',
  pid: 42,
  composedCommand: 'cmd && cmd',
  createdAt: 1700000000,
} as const satisfies SubSession;
