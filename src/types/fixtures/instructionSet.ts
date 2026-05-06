// See `session.ts` for why this fixture is `.ts` rather than `.json`
// (TypeScript widens JSON-import literal types, defeating `satisfies`
// against `InstructionSet`'s tagged-union `tool` field).

import type { InstructionSet } from '../arborist';

export const instructionSetFixture = {
  id: 'claude-default',
  name: 'Claude default',
  tool: 'claude',
  filePath: '/cfg/instructions/claude-default.md',
  isDefault: true,
} as const satisfies InstructionSet;
