import { describe, expect, it } from 'vitest';

import { createBuiltinsRegistry } from './builtins';
import type { CustomProcessDef } from '@/types/arborist';

const makeDef = (id: string, command: string): CustomProcessDef => ({
  id,
  name: id,
  kind: 'application',
  command,
  enabled: true,
});

describe('createBuiltinsRegistry()', () => {
  it('registers every built-in plugin kind in deterministic order', () => {
    const registry = createBuiltinsRegistry();
    expect(registry.ai().map((p) => p.id)).toEqual(['claude', 'copilot']);
    expect(registry.customProcesses().map((p) => p.id)).toEqual(['vscode', 'explorer']);
    expect(registry.widgets().map((w) => w.id)).toEqual(['git-status', 'ai-usage']);
  });

  it('uses built-in custom-process descriptors for command-shape matching', () => {
    const registry = createBuiltinsRegistry();
    const vscode = registry.customProcesses().find((p) => p.id === 'vscode');
    const explorer = registry.customProcesses().find((p) => p.id === 'explorer');

    expect(vscode?.matches(makeDef('custom-code', 'code .'))).toBe(true);
    expect(vscode?.matches(makeDef('custom-open', 'open .'))).toBe(false);
    expect(explorer?.matches(makeDef('custom-explorer', 'explorer .'))).toBe(true);
  });
});
