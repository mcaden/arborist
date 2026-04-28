// Hand-written TypeScript mirrors of the Rust types in
// `src-tauri/src/types.rs`. Each interface carries a `MIRROR:` marker
// pointing at the canonical Rust definition. **When you change a Rust struct,
// update the matching interface here in the same commit.**
//
// Drift is enforced at test time by `grove.test.ts`: every fixture under
// `./fixtures/` must satisfy its declared TS type *and* must have exactly
// the same key set. A renamed Rust field will fail the fixture round-trip on
// the Rust side and the satisfies-check here on the TS side.

// MIRROR: src-tauri/src/types.rs::SessionId
export type SessionId = string;

// MIRROR: src-tauri/src/types.rs::InstructionSetId
export type InstructionSetId = string;

// MIRROR: src-tauri/src/types.rs::Tool
export type Tool = 'claude' | 'copilot';

// MIRROR: src-tauri/src/types.rs::SessionStatus
export type SessionStatus = 'starting' | 'running' | 'exited' | 'error';

// MIRROR: src-tauri/src/types.rs::TempFileSpec
export interface TempFileSpec {
  path: string;
  contents: string;
}

// MIRROR: src-tauri/src/types.rs::Session
// Backend-only record. Not sent to the frontend in normal flows; included
// here so persistence/debug tooling can type it correctly.
export interface Session {
  id: SessionId;
  tool: Tool;
  worktreePath: string;
  worktreeName: string;
  label: string;
  instructionSetId: InstructionSetId;
  composedCommand: string;
  status: SessionStatus;
  pid?: number;
  createdAt: number;
  tabIndex: number;
  tempFiles: TempFileSpec[];
}

// MIRROR: src-tauri/src/types.rs::SessionView
// Frontend-facing projection: omits `composedCommand` and `tempFiles`.
export interface SessionView {
  id: SessionId;
  tool: Tool;
  worktreePath: string;
  worktreeName: string;
  label: string;
  instructionSetId: InstructionSetId;
  status: SessionStatus;
  pid?: number;
  createdAt: number;
  tabIndex: number;
}

// MIRROR: src-tauri/src/types.rs::InstructionSet
export interface InstructionSet {
  id: InstructionSetId;
  name: string;
  tool: Tool;
  filePath: string;
  isDefault: boolean;
}

// MIRROR: src-tauri/src/types.rs::DefaultInstructionSets
export interface DefaultInstructionSets {
  claude: InstructionSetId;
  copilot: InstructionSetId;
}

// MIRROR: src-tauri/src/types.rs::AppConfig
export interface AppConfig {
  defaultInstructionSets: DefaultInstructionSets;
  instructionSetsDir: string;
  worktreeRoots: string[];
  prelaunchCommands: string[];
  worktreePrelaunchCommands: Record<string, string[]>;
  lastOpenSessions: SessionId[];
  tabOrder: SessionId[];
}

// MIRROR: src-tauri/src/types.rs::PartialDefaultInstructionSets
export interface PartialDefaultInstructionSets {
  claude?: InstructionSetId;
  copilot?: InstructionSetId;
}

// MIRROR: src-tauri/src/types.rs::PartialAppConfig
// Every field optional so Phase 4's `config_set` can deep-merge updates.
export interface PartialAppConfig {
  defaultInstructionSets?: PartialDefaultInstructionSets;
  instructionSetsDir?: string;
  worktreeRoots?: string[];
  prelaunchCommands?: string[];
  worktreePrelaunchCommands?: Record<string, string[]>;
  lastOpenSessions?: SessionId[];
  tabOrder?: SessionId[];
}

// MIRROR: src-tauri/src/types.rs::AppError
// Wire shape of every error coming from a Tauri command. The frontend may
// branch on `code`; the strings come from `Error::code()` in Rust.
export interface AppError {
  code: string;
  message: string;
}
