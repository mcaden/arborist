import { describe, expect, it } from 'vitest';

import { createBuiltinsRegistry } from './builtins';

describe('createBuiltinsRegistry()', () => {
  it('registers dashboard widgets in deterministic order', () => {
    const registry = createBuiltinsRegistry();
    expect(registry.widgets().map((w) => w.id)).toEqual(['git-status', 'ai-usage']);
  });
});
