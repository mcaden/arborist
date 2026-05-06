// `.ts` rather than `.json` so the `status` discriminator keeps its
// literal type.

import type { SubSessionStatusEvent } from '../arborist';

export const subSessionStatusEventFixture = {
  id: '00000000-0000-0000-0000-000000000aaa',
  status: 'running',
  pid: 12345,
} as const satisfies SubSessionStatusEvent;
