// See `session.ts` for why this fixture is `.ts` rather than `.json`
// (TypeScript widens JSON-import literal types, defeating `satisfies`
// against `SessionStatusEvent`'s tagged-union `status` field).

import type { SessionStatusEvent } from '../arborist';

export const sessionStatusEventFixture = {
  sessionId: '8a3e1c5e-2b41-4b31-9dc7-1d77a3a51f00',
  status: 'running',
} as const satisfies SessionStatusEvent;
