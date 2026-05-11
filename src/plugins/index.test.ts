import { describe, expect, it } from 'vitest';

import { createRegistry, PluginRegisterError, type AiPlugin, type CustomProcessPlugin, type DashboardWidgetPlugin } from './index';

const makeAi = (id: string): AiPlugin => ({
  id,
  displayName: id,
  defaultProgram: id,
  defaultInstructionSetPath: `${id}-default.md`,
});

const makeProc = (id: string, matchId: string): CustomProcessPlugin => ({
  id,
  displayName: id,
  matches: (def) => def.id === matchId,
  supportedOnPlatform: () => true,
});

const makeWidget = (id: string): DashboardWidgetPlugin => ({
  id,
  displayName: id,
  order: 0,
  Component: () => null,
});

describe('createRegistry()', () => {
  it('returns a registry with the expected shape', () => {
    const r = createRegistry();
    expect(typeof r.registerAi).toBe('function');
    expect(typeof r.registerCustomProcess).toBe('function');
    expect(typeof r.registerWidget).toBe('function');
    expect(r.ai()).toEqual([]);
    expect(r.customProcesses()).toEqual([]);
    expect(r.widgets()).toEqual([]);
  });

  it('registers AI plugins in insertion order and supports id lookup', () => {
    const r = createRegistry();
    r.registerAi(makeAi('alpha'));
    r.registerAi(makeAi('beta'));
    expect(r.ai().map((p) => p.id)).toEqual(['alpha', 'beta']);
    expect(r.aiById('alpha')?.displayName).toBe('alpha');
    expect(r.aiById('missing')).toBeUndefined();
  });

  it('throws PluginRegisterError on duplicate AI ids', () => {
    const r = createRegistry();
    r.registerAi(makeAi('claude'));
    expect(() => r.registerAi(makeAi('claude'))).toThrow(PluginRegisterError);
  });

  it('throws PluginRegisterError on duplicate custom-process ids', () => {
    const r = createRegistry();
    r.registerCustomProcess(makeProc('vscode', 'vscode'));
    expect(() => r.registerCustomProcess(makeProc('vscode', 'vscode'))).toThrow(PluginRegisterError);
  });

  it('throws PluginRegisterError on duplicate widget ids', () => {
    const r = createRegistry();
    r.registerWidget(makeWidget('git-status'));
    expect(() => r.registerWidget(makeWidget('git-status'))).toThrow(PluginRegisterError);
  });

  it('customProcessForDef returns first matching plugin or undefined', () => {
    const r = createRegistry();
    r.registerCustomProcess(makeProc('vscode', 'vscode'));
    expect(r.customProcessForDef({ id: 'vscode', command: 'code .' })?.id).toBe('vscode');
    expect(r.customProcessForDef({ id: 'shell', command: 'pwsh' })).toBeUndefined();
  });

  it('customProcessForDef skips plugins that are not supported on the current platform', () => {
    // Mirrors the Windows-Explorer-on-Linux case from #97: an unsupported plugin must not "win" the lookup, even if it was registered first and
    // matches the def. The supported plugin registered afterwards should be returned.
    const r = createRegistry();
    r.registerCustomProcess({
      id: 'explorer',
      displayName: 'Explorer',
      matches: (def) => def.id === 'shared',
      supportedOnPlatform: () => false,
    });
    r.registerCustomProcess({
      id: 'fallback',
      displayName: 'Fallback',
      matches: (def) => def.id === 'shared',
      supportedOnPlatform: () => true,
    });
    expect(r.customProcessForDef({ id: 'shared', command: 'noop' })?.id).toBe('fallback');
  });

  it('accessors return defensive copies so external mutation cannot desync the registry', () => {
    const r = createRegistry();
    r.registerAi(makeAi('claude'));
    r.registerCustomProcess(makeProc('vscode', 'vscode'));
    r.registerWidget(makeWidget('git-status'));

    // Mutating the returned arrays must not affect what the registry surfaces on subsequent calls. `readonly` is compile-time only; this test
    // pins down the runtime behaviour.
    (r.ai() as AiPlugin[]).length = 0;
    (r.customProcesses() as CustomProcessPlugin[]).pop();
    (r.widgets() as DashboardWidgetPlugin[]).push(makeWidget('rogue'));

    expect(r.ai().map((p) => p.id)).toEqual(['claude']);
    expect(r.customProcesses().map((p) => p.id)).toEqual(['vscode']);
    expect(r.widgets().map((p) => p.id)).toEqual(['git-status']);
  });
});
