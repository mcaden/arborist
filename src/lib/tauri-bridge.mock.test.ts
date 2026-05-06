// Type-level smoke test: importing the mock should yield exports
// structurally compatible with the real bridge so that test files can
// substitute one for the other via `vi.mock`. The `satisfies` assertion in
// `tauri-bridge.mock.ts` already guards this at compile time; this file
// gives us a runtime trip-wire and a place to assert `resetBridgeMocks`
// behaviour.

import { describe, expect, it, vi } from 'vitest';

import * as mock from './tauri-bridge.mock';

describe('tauri-bridge.mock', () => {
  it('exports vi.fn mocks for every bridge member', () => {
    const expected = [
      'ping',
      'sessionCreate',
      'sessionList',
      'sessionClose',
      'sessionFocus',
      'sessionResize',
      'sessionInput',
      'sessionRestart',
      'frontendReady',
      'configGet',
      'configSet',
      'instructionsList',
      'onSessionOutput',
      'onSessionStatus',
    ] as const;
    for (const name of expected) {
      const fn = mock[name];
      expect(vi.isMockFunction(fn), `${name} must be a vi.fn`).toBe(true);
    }
  });

  it('resetBridgeMocks clears history and restores default impls', async () => {
    mock.ping.mockResolvedValueOnce('overridden');
    await mock.ping();
    expect(mock.ping).toHaveBeenCalledTimes(1);

    mock.resetBridgeMocks();

    expect(mock.ping).toHaveBeenCalledTimes(0);
    await expect(mock.ping()).resolves.toBe('pong');
    // Phase 7: sessionList default became Promise.resolve([]).
    await expect(mock.sessionList()).resolves.toEqual([]);
    // sessionCreate is the only remaining stub that rejects by default
    // (callers should opt-in by setting a return value per test).
    await expect(
      mock.sessionCreate({
        tool: 'claude',
        worktreePath: '/wt',
        instructionSetId: 'claude-default',
        cols: 80,
        rows: 24,
      }),
    ).rejects.toThrow('not implemented');
  });
});
