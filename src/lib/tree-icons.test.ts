import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { WORKTREE_ICON_COUNT, getTreeIconUrl } from './tree-icons';

// `getTreeIconUrl` lives in a module-scoped state (`WARNED_IDS`) on purpose, so the dedup behavior is observable across calls within a single page
// load. Tests can't reset that Set without re-importing the module each test (which would also re-evaluate `import.meta.glob` and risk inconsistency
// with the production-shaped icon map). Instead, every test below uses a *unique* invalid id so cross-test pollution doesn't matter — repeated calls
// with the same id are the assertion target, not absolute call counts.
describe('getTreeIconUrl', () => {
  let warnSpy: ReturnType<typeof vi.spyOn>;
  let errorSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    warnSpy = vi.spyOn(console, 'warn').mockImplementation(() => undefined);
    errorSpy = vi.spyOn(console, 'error').mockImplementation(() => undefined);
  });

  afterEach(() => {
    warnSpy.mockRestore();
    errorSpy.mockRestore();
  });

  it('returns a non-empty URL for valid iconIds in 1..=WORKTREE_ICON_COUNT', () => {
    expect(WORKTREE_ICON_COUNT).toBeGreaterThan(0);
    for (let id = 1; id <= WORKTREE_ICON_COUNT; id += 1) {
      const url = getTreeIconUrl(id);
      expect(url).toBeTypeOf('string');
      expect(url.length).toBeGreaterThan(0);
    }
    expect(warnSpy).not.toHaveBeenCalled();
    expect(errorSpy).not.toHaveBeenCalled();
  });

  it('falls back to a non-empty URL for out-of-range iconIds', () => {
    const url = getTreeIconUrl(9001);
    expect(url).toBeTypeOf('string');
    expect(url.length).toBeGreaterThan(0);
  });

  it('warns at most once per unique invalid iconId across repeated calls (dedup guard)', () => {
    // Use a high id no other test references so the Set entry is fresh for this assertion.
    const badId = 9100;
    const before = warnSpy.mock.calls.length;
    getTreeIconUrl(badId);
    getTreeIconUrl(badId);
    getTreeIconUrl(badId);
    const callsForBadId = warnSpy.mock.calls
      .slice(before)
      .filter((args) => typeof args[0] === 'string' && (args[0] as string).includes(`iconId ${badId}`));
    expect(callsForBadId).toHaveLength(1);
  });

  it('warns separately for each distinct invalid iconId', () => {
    const idA = 9200;
    const idB = 9201;
    const before = warnSpy.mock.calls.length;
    getTreeIconUrl(idA);
    getTreeIconUrl(idB);
    getTreeIconUrl(idA); // duplicate — must not produce a second warn
    const newCalls = warnSpy.mock.calls.slice(before);
    const sawA = newCalls.some((args) => typeof args[0] === 'string' && (args[0] as string).includes(`iconId ${idA}`));
    const sawB = newCalls.some((args) => typeof args[0] === 'string' && (args[0] as string).includes(`iconId ${idB}`));
    expect(sawA).toBe(true);
    expect(sawB).toBe(true);
    // Exactly one warn for A and one for B from this test, so two new calls total touching these ids.
    const idTouchingCalls = newCalls.filter(
      (args) => typeof args[0] === 'string' && ((args[0] as string).includes(`iconId ${idA}`) || (args[0] as string).includes(`iconId ${idB}`)),
    );
    expect(idTouchingCalls).toHaveLength(2);
  });

  it('dedupes NaN inputs across repeated calls (NaN !== NaN guard)', () => {
    // A naive `Set<number>` would never contain NaN because `Set.has(NaN)` is true but every NaN is distinct under `===` — without the normalisation
    // in `getTreeIconUrl`, repeated NaN calls would warn forever. This test pins the normalised behavior.
    const before = warnSpy.mock.calls.length;
    getTreeIconUrl(Number.NaN);
    getTreeIconUrl(Number.NaN);
    getTreeIconUrl(Number.NaN);
    const nanCalls = warnSpy.mock.calls.slice(before).filter((args) => typeof args[0] === 'string' && (args[0] as string).includes('iconId NaN'));
    expect(nanCalls).toHaveLength(1);
  });
});
