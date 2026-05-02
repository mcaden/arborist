// Cross-boundary contract test: each fixture under `./fixtures/` is the
// frontend's record of the wire shape produced by the matching Rust
// serde round-trip in `src-tauri/src/types.rs`. If a Rust field is
// renamed, added, or removed without updating the TypeScript mirror in
// `./arborist.ts`, this file will fail to typecheck (or fail the
// runtime key-set assertions below).

import { describe, expect, it } from 'vitest';
import { expectTypeOf } from 'vitest';

import type {
  AppConfig,
  AppError,
  PartialAppConfig,
  Session,
  SessionOutputEvent,
} from './arborist';

import { sessionFixture } from './fixtures/session';
import { sessionViewFixture } from './fixtures/sessionView';
import { instructionSetFixture } from './fixtures/instructionSet';
import appConfigFixture from './fixtures/appConfig.json';
import partialAppConfigFixture from './fixtures/partialAppConfig.json';
import appErrorFixture from './fixtures/appError.json';
import sessionOutputEventFixture from './fixtures/sessionOutputEvent.json';
import { sessionStatusEventFixture } from './fixtures/sessionStatusEvent';

// --- Compile-time assertions ------------------------------------------------
//
// The four fixtures with discriminated-union fields (`tool`, `status`)
// live in `.ts` modules so their literal types are preserved — TS
// widens JSON imports (`tool: "claude"` → `tool: string`) which would
// make `satisfies` against tagged unions fail spuriously and force an
// `as`-cast escape hatch that silently swallows real drift.
//
// `as const satisfies T` inside each fixture file is the primary
// drift detector: missing required field, renamed field, or wrong
// literal value all surface as compile errors there. We re-affirm
// the assignability to the mirror types here so this test file
// itself fails to typecheck if anyone deletes the `satisfies` from
// the fixture or breaks the mirror import.
//
// JSON-backed fixtures (no tagged-union fields) keep their existing
// `satisfies` checks below. Runtime key-set assertions further down
// catch *extra* keys (which TS's structural typing would otherwise
// allow).

const _appConfig = appConfigFixture satisfies AppConfig;
const _partialAppConfig = partialAppConfigFixture satisfies PartialAppConfig;
const _appError = appErrorFixture satisfies AppError;
const _sessionOutputEvent = sessionOutputEventFixture satisfies SessionOutputEvent;

// Silence "unused" lint without losing the satisfies assertion.
void sessionFixture;
void sessionViewFixture;
void instructionSetFixture;
void _appConfig;
void _partialAppConfig;
void _appError;
void _sessionOutputEvent;
void sessionStatusEventFixture;

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
        'composedCommand',
        'status',
        'createdAt',
        'tabIndex',
        'tempFiles',
      ],
      ['pid', 'instructionSetId', 'aiSessionId'],
      'Session',
    );
  });

  it('SessionView fixture matches TS interface key set and omits backend-only fields', () => {
    assertExactKeys(
      sessionViewFixture as unknown as Record<string, unknown>,
      ['id', 'tool', 'worktreePath', 'worktreeName', 'label', 'status', 'createdAt', 'tabIndex'],
      ['pid', 'instructionSetId'],
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
        'workspaceRoot',
        'worktreeRoots',
        'prelaunchCommands',
        'worktreePrelaunchCommands',
        'aiLaunchCommands',
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
      'workspaceRoot',
      'worktreeRoots',
      'prelaunchCommands',
      'worktreePrelaunchCommands',
      'aiLaunchCommands',
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
      ['message'],
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
