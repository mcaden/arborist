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
});
