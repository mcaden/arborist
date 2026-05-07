// `.ts` rather than `.json` so the `kind` discriminator (`'terminal' |
// 'application'`) keeps its literal type — JSON imports widen to
// `string`, defeating `satisfies CustomProcessDef`.

import type { CustomProcessDef } from '../arborist';

export const customProcessDefFixture = {
  id: 'vscode',
  name: 'VS Code',
  kind: 'application',
  command: 'code .',
  enabled: true,
  icon: 'vscode',
} as const satisfies CustomProcessDef;
