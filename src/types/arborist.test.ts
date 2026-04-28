// Cross-boundary contract test: each JSON fixture under `./fixtures/` is
// produced by the matching Rust serde round-trip in
// `src-tauri/src/types.rs`. If a Rust field is renamed, added, or removed
// without updating the TypeScript mirror in `./arborist.ts`, this file will
// fail to typecheck (or fail the runtime key-set assertions below).

import { describe, expect, it } from 'vitest';
import { expectTypeOf } from 'vitest';

import type {
  AppConfig,
  AppError,
  InstructionSet,
  PartialAppConfig,
  Session,
  SessionOutputEvent,
  SessionStatusEvent,
  SessionView,
} from './arborist';

import sessionFixture from './fixtures/session.json';
import sessionViewFixture from './fixtures/sessionView.json';
import instructionSetFixture from './fixtures/instructionSet.json';
import appConfigFixture from './fixtures/appConfig.json';
import partialAppConfigFixture from './fixtures/partialAppConfig.json';
import appErrorFixture from './fixtures/appError.json';
import sessionOutputEventFixture from './fixtures/sessionOutputEvent.json';
import sessionStatusEventFixture from './fixtures/sessionStatusEvent.json';

// --- Compile-time assertions ------------------------------------------------
//
// `satisfies` checks that the imported (literal-typed) fixture is assignable
// to the mirror interface. Missing required fields → compile error.
// Renamed fields → compile error. This is the primary drift detector.
//
// Note: TypeScript's structural typing means a fixture with *extra* keys
// would still satisfy the interface, so we additionally compare key sets at
// runtime below to catch backend additions the frontend hasn't mirrored.

const _session = sessionFixture satisfies Session;
const _sessionView = sessionViewFixture satisfies SessionView;
const _instructionSet = instructionSetFixture satisfies InstructionSet;
const _appConfig = appConfigFixture satisfies AppConfig;
const _partialAppConfig = partialAppConfigFixture satisfies PartialAppConfig;
const _appError = appErrorFixture satisfies AppError;
const _sessionOutputEvent = sessionOutputEventFixture satisfies SessionOutputEvent;
const _sessionStatusEvent = sessionStatusEventFixture satisfies SessionStatusEvent;

// Silence "unused" lint without losing the satisfies assertion.
void _session;
void _sessionView;
void _instructionSet;
void _appConfig;
void _partialAppConfig;
void _appError;
void _sessionOutputEvent;
void _sessionStatusEvent;

// --- Runtime key-set assertions --------------------------------------------

/**
 * Required-key set the TS interface declares. `expected` lists every
 * non-optional field of the corresponding interface. Optional fields are
 * checked separately so a fixture may legitimately omit them.
 */
function assertExactKeys(
  fixture: Record<string, unknown>,
  required: readonly string[],
  optional: readonly string[],
  label: string,
): void {
  const fixtureKeys = new Set(Object.keys(fixture));
  const allowed = new Set([...required, ...optional]);

  const missing = required.filter((k) => !fixtureKeys.has(k));
  expect(missing, `${label}: fixture missing required keys`).toEqual([]);

  const unexpected = [...fixtureKeys].filter((k) => !allowed.has(k));
  expect(unexpected, `${label}: fixture has keys not declared in TS mirror`).toEqual([]);
}

describe('arborist type mirrors', () => {
  it('Session fixture matches TS interface key set', () => {
    assertExactKeys(
      sessionFixture as unknown as Record<string, unknown>,
      [
        'id',
        'tool',
        'worktreePath',
        'worktreeName',
        'label',
        'instructionSetId',
        'composedCommand',
        'status',
        'createdAt',
        'tabIndex',
        'tempFiles',
      ],
      ['pid'],
      'Session',
    );
  });

  it('SessionView fixture matches TS interface key set and omits backend-only fields', () => {
    assertExactKeys(
      sessionViewFixture as unknown as Record<string, unknown>,
      [
        'id',
        'tool',
        'worktreePath',
        'worktreeName',
        'label',
        'instructionSetId',
        'status',
        'createdAt',
        'tabIndex',
      ],
      ['pid'],
      'SessionView',
    );
    expect(sessionViewFixture).not.toHaveProperty('composedCommand');
    expect(sessionViewFixture).not.toHaveProperty('tempFiles');
  });

  it('InstructionSet fixture matches TS interface key set', () => {
    assertExactKeys(
      instructionSetFixture as unknown as Record<string, unknown>,
      ['id', 'name', 'tool', 'filePath', 'isDefault'],
      [],
      'InstructionSet',
    );
  });

  it('AppConfig fixture matches TS interface key set', () => {
    assertExactKeys(
      appConfigFixture as unknown as Record<string, unknown>,
      [
        'configVersion',
        'defaultInstructionSets',
        'instructionSetsDir',
        'worktreeRoots',
        'prelaunchCommands',
        'worktreePrelaunchCommands',
        'lastOpenSessions',
        'tabOrder',
        'activeSessionId',
      ],
      [],
      'AppConfig',
    );
  });

  it('PartialAppConfig fixture only contains keys declared in TS mirror', () => {
    // Every PartialAppConfig key is optional; we just guard against typos.
    const allowed = new Set([
      'configVersion',
      'defaultInstructionSets',
      'instructionSetsDir',
      'worktreeRoots',
      'prelaunchCommands',
      'worktreePrelaunchCommands',
      'lastOpenSessions',
      'tabOrder',
      'activeSessionId',
    ]);
    const unexpected = Object.keys(partialAppConfigFixture).filter((k) => !allowed.has(k));
    expect(unexpected, 'PartialAppConfig: fixture has keys not declared in TS mirror').toEqual([]);
  });

  it('AppError fixture matches { code, message }', () => {
    assertExactKeys(
      appErrorFixture as unknown as Record<string, unknown>,
      ['code', 'message'],
      [],
      'AppError',
    );
  });

  it('SessionOutputEvent fixture matches TS interface key set', () => {
    assertExactKeys(
      sessionOutputEventFixture as unknown as Record<string, unknown>,
      ['sessionId', 'data'],
      [],
      'SessionOutputEvent',
    );
  });

  it('SessionStatusEvent fixture matches TS interface key set', () => {
    assertExactKeys(
      sessionStatusEventFixture as unknown as Record<string, unknown>,
      ['sessionId', 'status'],
      [],
      'SessionStatusEvent',
    );
  });

  it('Tool wire values are lowercase string literals', () => {
    expectTypeOf<Session['tool']>().toEqualTypeOf<'claude' | 'copilot'>();
  });

  it('SessionStatus wire values are lowercase string literals', () => {
    expectTypeOf<Session['status']>().toEqualTypeOf<'starting' | 'running' | 'exited' | 'error'>();
  });
});
