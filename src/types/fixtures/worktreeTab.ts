// `.ts` rather than `.json` so the `activeChildId.kind` discriminator keeps its literal type.

import type { ChildId, WorktreeTab } from '../arborist';

export const sessionChildIdFixture = {
  kind: 'session',
  id: '550e8400-e29b-41d4-a716-446655440000',
} as const satisfies ChildId;

export const subSessionChildIdFixture = {
  kind: 'subSession',
  id: '11111111-1111-1111-1111-111111111111',
} as const satisfies ChildId;

export const worktreeTabFixture = {
  id: '550e8400-e29b-41d4-a716-446655440001',
  path: '/repo/feature-x',
  name: 'feature-x',
  branch: 'feature-x',
  label: 'feature-x',
  tabIndex: 0,
  activeChildId: subSessionChildIdFixture,
} as const satisfies WorktreeTab;
