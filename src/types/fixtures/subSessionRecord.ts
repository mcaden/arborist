// `.ts` rather than `.json` so the `kind` discriminator keeps its
// literal type.

import type { SubSessionRecord } from '../arborist';

export const subSessionRecordFixture = {
  id: '11111111-1111-1111-1111-111111111111',
  parentWorktreeTabId: '550e8400-e29b-41d4-a716-446655440000',
  defId: 'shell',
  kind: 'terminal',
  label: 'Shell',
  composedCommand: 'cmd /c shell',
} as const satisfies SubSessionRecord;
