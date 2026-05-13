import { describe, expect, it } from 'vitest';

import { createBuiltinsRegistry } from './builtins';

describe('createBuiltinsRegistry()', () => {
  it('registers AI tools and dashboard widgets in deterministic order', () => {
    const registry = createBuiltinsRegistry();
    expect(registry.ai().map((p) => p.id)).toEqual(['claude', 'copilot']);
    expect(registry.widgets().map((w) => w.id)).toEqual(['git-status', 'ai-usage']);
  });
});
