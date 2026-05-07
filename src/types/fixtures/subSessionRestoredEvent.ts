// `.ts` rather than `.json` so the nested `kind` and `status`
// discriminators on the embedded `SubSession` keep their literal types.

import type { SubSessionRestoredEvent } from '../arborist';

export const subSessionRestoredEventFixture = {
  subSession: {
    id: '00000000-0000-0000-0000-000000000aaa',
    parentSessionId: '11111111-1111-1111-1111-11111111aaaa',
    defId: 'shell',
    kind: 'terminal',
    label: 'shell',
    status: 'starting',
    composedCommand: 'bash -l',
    createdAt: 1730390400,
  },
} as const satisfies SubSessionRestoredEvent;
