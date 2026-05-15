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
  ChildId,
  CustomProcessDef,
  PartialAppConfig,
  Session,
  SessionOutputEvent,
  SessionStatusEvent,
  SubSessionStatusEvent,
  SubSessionExitedEvent,
  SubSessionRestoredEvent,
  SessionView,
  SubSession,
  SubSessionRecord,
  WorktreeTab,
  WorkspaceSwitchArgs,
  WorkspaceSwitchResult,
} from './arborist';

import { sessionFixture } from './fixtures/session';
import { sessionViewFixture } from './fixtures/sessionView';
import { appConfigFixture } from './fixtures/appConfig';
import partialAppConfigFixture from './fixtures/partialAppConfig.json';
import appErrorFixture from './fixtures/appError.json';
import sessionOutputEventFixture from './fixtures/sessionOutputEvent.json';
import { sessionStatusEventFixture } from './fixtures/sessionStatusEvent';
import { customProcessDefFixture } from './fixtures/customProcessDef';
import { subSessionFixture } from './fixtures/subSession';
import { subSessionRecordFixture } from './fixtures/subSessionRecord';
import { subSessionStatusEventFixture } from './fixtures/subSessionStatusEvent';
import subSessionExitedEventFixture from './fixtures/subSessionExitedEvent.json';
import { subSessionRestoredEventFixture } from './fixtures/subSessionRestoredEvent';
import workspaceSwitchArgsFixture from './fixtures/workspaceSwitchArgs.json';
import { workspaceSwitchResultFixture } from './fixtures/workspaceSwitchResult';
import { sessionChildIdFixture, subSessionChildIdFixture, worktreeTabFixture } from './fixtures/worktreeTab';

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
// literal value all surface as compile errors there. We **also**
// re-assert assignability here with explicit `satisfies` clauses so
// this test file itself fails to typecheck if anyone deletes the
// `satisfies` from the fixture or breaks the mirror import. The
// previous `void <fixture>` form only consumed the value to silence
// the unused-import lint and did **not** re-check assignability —
// the drift detector was effectively defenseless against fixture-
// side weakening. Use `<fixture> satisfies T` (an expression-level
// operator) which preserves the fixture's narrowed `as const` type
// while enforcing the contract.
//
// JSON-backed fixtures get the same treatment. Runtime key-set
// assertions further down catch *extra* keys (which TS's structural
// typing would otherwise allow).

const _session = sessionFixture satisfies Session;
const _sessionView = sessionViewFixture satisfies SessionView;
const _sessionStatusEvent = sessionStatusEventFixture satisfies SessionStatusEvent;
const _appConfig = appConfigFixture satisfies AppConfig;
const _partialAppConfig = partialAppConfigFixture satisfies PartialAppConfig;
const _appError = appErrorFixture satisfies AppError;
const _sessionOutputEvent = sessionOutputEventFixture satisfies SessionOutputEvent;
const _customProcessDef = customProcessDefFixture satisfies CustomProcessDef;
const _subSession = subSessionFixture satisfies SubSession;
const _subSessionRecord = subSessionRecordFixture satisfies SubSessionRecord;
const _subSessionStatusEvent = subSessionStatusEventFixture satisfies SubSessionStatusEvent;
const _subSessionExitedEvent = subSessionExitedEventFixture satisfies SubSessionExitedEvent;
const _subSessionRestoredEvent = subSessionRestoredEventFixture satisfies SubSessionRestoredEvent;
const _workspaceSwitchArgs = workspaceSwitchArgsFixture satisfies WorkspaceSwitchArgs;
const _workspaceSwitchResult = workspaceSwitchResultFixture satisfies WorkspaceSwitchResult;
const _sessionChildId = sessionChildIdFixture satisfies ChildId;
const _subSessionChildId = subSessionChildIdFixture satisfies ChildId;
const _worktreeTab = worktreeTabFixture satisfies WorktreeTab;

// Silence "unused" lint on the locally-bound aliases. The `satisfies`
// check above is what enforces drift — these voids carry no contract.
void _session;
void _sessionView;
void _sessionStatusEvent;
void _appConfig;
void _partialAppConfig;
void _appError;
void _sessionOutputEvent;
void _customProcessDef;
void _subSession;
void _subSessionRecord;
void _subSessionStatusEvent;
void _subSessionExitedEvent;
void _subSessionRestoredEvent;
void _workspaceSwitchArgs;
void _workspaceSwitchResult;
void _sessionChildId;
void _subSessionChildId;
void _worktreeTab;

// --- Runtime key-set assertions --------------------------------------------

/**
 * Required-key set the TS interface declares. `expected` lists every
 * non-optional field of the corresponding interface. Optional fields are
 * checked separately so a fixture may legitimately omit them.
 */
function assertExactKeys(fixture: Record<string, unknown>, required: readonly string[], optional: readonly string[], label: string): void {
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
      ['id', 'tool', 'worktreePath', 'worktreeName', 'label', 'composedCommand', 'status', 'createdAt', 'tabIndex', 'tempFiles'],
      ['pid', 'aiSessionId'],
      'Session',
    );
  });

  it('SessionView fixture matches TS interface key set and omits backend-only fields', () => {
    assertExactKeys(
      sessionViewFixture as unknown as Record<string, unknown>,
      ['id', 'tool', 'worktreePath', 'worktreeName', 'label', 'status', 'createdAt', 'tabIndex'],
      ['pid'],
      'SessionView',
    );
    expect(sessionViewFixture).not.toHaveProperty('composedCommand');
    expect(sessionViewFixture).not.toHaveProperty('tempFiles');
  });

  it('AppConfig fixture matches TS interface key set', () => {
    assertExactKeys(
      appConfigFixture as unknown as Record<string, unknown>,
      [
        'configVersion',
        'workspaceRoot',
        'worktreeRoots',
        'worktreePrepCommands',
        'aiLaunchCommands',
        'repoCommandTrust',
        'pluginSettings',
        'lastOpenSessions',
        'tabOrder',
        'activeSessionId',
        'customProcesses',
        'lastOpenSubSessions',
        'worktreeTabs',
        'worktreeTabOrder',
        'activeWorktreeTabId',
        'theme',
      ],
      [],
      'AppConfig',
    );
  });

  it('PartialAppConfig fixture only contains keys declared in TS mirror', () => {
    // Every PartialAppConfig key is optional; we just guard against typos.
    const allowed = new Set([
      'configVersion',
      'workspaceRoot',
      'worktreeRoots',
      'worktreePrepCommands',
      'aiLaunchCommands',
      'pluginSettings',
      'lastOpenSessions',
      'tabOrder',
      'activeSessionId',
      'customProcesses',
      'lastOpenSubSessions',
      'worktreeTabs',
      'worktreeTabOrder',
      'activeWorktreeTabId',
      'theme',
    ]);
    const unexpected = Object.keys(partialAppConfigFixture).filter((k) => !allowed.has(k));
    expect(unexpected, 'PartialAppConfig: fixture has keys not declared in TS mirror').toEqual([]);
  });

  it('AppError fixture matches { code, message }', () => {
    assertExactKeys(appErrorFixture as unknown as Record<string, unknown>, ['code', 'message'], [], 'AppError');
  });

  it('SessionOutputEvent fixture matches TS interface key set', () => {
    assertExactKeys(sessionOutputEventFixture as unknown as Record<string, unknown>, ['sessionId', 'data'], [], 'SessionOutputEvent');
  });

  it('SessionStatusEvent fixture matches TS interface key set', () => {
    assertExactKeys(sessionStatusEventFixture as unknown as Record<string, unknown>, ['sessionId', 'status'], ['message'], 'SessionStatusEvent');
  });

  it('WorkspaceSwitchArgs fixture matches TS interface key set', () => {
    assertExactKeys(workspaceSwitchArgsFixture as unknown as Record<string, unknown>, ['path'], [], 'WorkspaceSwitchArgs');
  });

  it('WorkspaceSwitchResult fixture matches TS interface key set', () => {
    assertExactKeys(
      workspaceSwitchResultFixture as unknown as Record<string, unknown>,
      ['workspaceRoot', 'noOp', 'config', 'sessions'],
      [],
      'WorkspaceSwitchResult',
    );
  });

  it('Tool wire values are lowercase string literals', () => {
    expectTypeOf<Session['tool']>().toEqualTypeOf<'claude' | 'copilot'>();
  });

  it('SessionStatus wire values are lowercase string literals', () => {
    expectTypeOf<Session['status']>().toEqualTypeOf<'starting' | 'running' | 'exited' | 'error'>();
  });

  it('CustomProcessDef fixture matches TS interface key set', () => {
    assertExactKeys(
      customProcessDefFixture as unknown as Record<string, unknown>,
      ['id', 'name', 'kind', 'command', 'enabled'],
      ['icon'],
      'CustomProcessDef',
    );
  });

  it('SubSession fixture matches TS interface key set', () => {
    assertExactKeys(
      subSessionFixture as unknown as Record<string, unknown>,
      ['id', 'parentWorktreeTabId', 'defId', 'kind', 'label', 'status', 'composedCommand', 'createdAt'],
      ['pid'],
      'SubSession',
    );
  });

  it('SubSessionRecord fixture matches TS interface key set', () => {
    assertExactKeys(
      subSessionRecordFixture as unknown as Record<string, unknown>,
      ['id', 'parentWorktreeTabId', 'defId', 'kind', 'label'],
      ['composedCommand'],
      'SubSessionRecord',
    );
  });

  it('SubSessionStatusEvent fixture matches TS interface key set', () => {
    assertExactKeys(
      subSessionStatusEventFixture as unknown as Record<string, unknown>,
      ['id', 'status'],
      ['pid', 'message'],
      'SubSessionStatusEvent',
    );
  });

  it('SubSessionExitedEvent fixture matches TS interface key set', () => {
    assertExactKeys(subSessionExitedEventFixture as unknown as Record<string, unknown>, ['id'], ['exitCode'], 'SubSessionExitedEvent');
  });

  it('SubSessionRestoredEvent fixture matches TS interface key set', () => {
    assertExactKeys(subSessionRestoredEventFixture as unknown as Record<string, unknown>, ['subSession'], [], 'SubSessionRestoredEvent');
  });

  it('ChildId fixtures match TS discriminated-union key set', () => {
    assertExactKeys(sessionChildIdFixture as unknown as Record<string, unknown>, ['kind', 'id'], [], 'ChildId(session)');
    assertExactKeys(subSessionChildIdFixture as unknown as Record<string, unknown>, ['kind', 'id'], [], 'ChildId(subSession)');
  });

  it('WorktreeTab fixture matches TS interface key set', () => {
    assertExactKeys(
      worktreeTabFixture as unknown as Record<string, unknown>,
      ['id', 'path', 'name', 'label', 'tabIndex', 'iconId'],
      ['branch', 'activeChildId'],
      'WorktreeTab',
    );
  });

  it('CustomProcessKind wire values are lowercase string literals', () => {
    expectTypeOf<CustomProcessDef['kind']>().toEqualTypeOf<'terminal' | 'application'>();
  });

  it('SubSessionStatus wire values are lowercase string literals', () => {
    expectTypeOf<SubSession['status']>().toEqualTypeOf<'starting' | 'running' | 'exited' | 'error'>();
  });

  it('ChildId wire values are discriminated by kind', () => {
    expectTypeOf<ChildId['kind']>().toEqualTypeOf<'session' | 'subSession'>();
  });
});
