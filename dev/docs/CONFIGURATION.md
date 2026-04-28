# Grove configuration

Grove keeps all persistent state in two JSON files inside the OS-specific
**app data directory** that Tauri provides. There is no in-app settings UI in
v1 — settings are edited by hand (with the app shut down) and reloaded on
next launch. This document describes the file layout, the minimum valid
`config.json`, and what to do when Grove quarantines a corrupt file.

## On-disk layout

| OS      | Path                                                       |
| ------- | ---------------------------------------------------------- |
| Windows | `%APPDATA%\com.grove.app\` (typically `C:\Users\<you>\AppData\Roaming\com.grove.app\`) |
| macOS   | `~/Library/Application Support/com.grove.app/`             |
| Linux   | `$XDG_DATA_HOME/com.grove.app/` or `~/.local/share/com.grove.app/` |

> The leaf directory name is taken from the Tauri `identifier` field in
> `src-tauri/tauri.conf.json`. If that value changes the directory changes
> with it.

Inside that directory:

| File            | Purpose                                                                                |
| --------------- | -------------------------------------------------------------------------------------- |
| `config.json`   | The user-editable [`AppConfig`](../../src-tauri/src/types.rs).                          |
| `sessions.json` | Backend-only mirror of every persisted [`Session`](../../src-tauri/src/types.rs).       |

You almost never need to touch `sessions.json` by hand — Grove rewrites it on
every session create/close/restart. If you do, shut Grove down first.

## Minimum valid `config.json`

```json
{
  "configVersion": 2,
  "defaultInstructionSets": {
    "claude": "claude-default",
    "copilot": "copilot-default"
  },
  "instructionSetsDir": "/absolute/path/to/instructions",
  "worktreeRoots": ["/absolute/path/to/repo"],
  "prelaunchCommands": [],
  "worktreePrelaunchCommands": {},
  "lastOpenSessions": [],
  "tabOrder": []
}
```

Field notes:

- `configVersion` — schema version of the file. Currently `2` (see
  `CONFIG_VERSION_CURRENT` in `src-tauri/src/types.rs`). Bumped only when
  the on-disk shape changes; older versions are quarantined (see below).
- `instructionSetsDir` — must be an **absolute** path that points at an
  existing directory. The path is canonicalized on load (symlinks resolved,
  `..` collapsed). Relative values are rejected when written via the
  `config_set` command and silently cleared when read from `config.json`.
- `worktreeRoots[]` — same rules as `instructionSetsDir`. Entries that no
  longer exist on disk are dropped (with a warning) on load.
- `prelaunchCommands[]` — global commands joined with `&&` before each CLI
  launch (SPEC §5.6). They run as the user; review them carefully.
- `worktreePrelaunchCommands` — per-worktree overrides. Keys are
  canonicalized worktree paths; entries whose paths don't canonicalize to an
  existing directory are dropped on load.
- `defaultInstructionSets.{claude,copilot}` — IDs of the default instruction
  set per tool. The ID is the filename (without the `.md` extension) of the
  file inside `instructionSetsDir`. If the configured ID isn't present, the
  loader falls back to the discovered default for that tool (see below).
- `lastOpenSessions` / `tabOrder` — managed by Grove; you can leave them
  empty when bootstrapping.

## Instruction set discovery

`instructionSetsDir` is scanned for `*.md` files at startup and whenever the
`instructions_list` command is invoked. The discovery rules are:

1. Each candidate file is canonicalized; files whose canonical path falls
   **outside** `instructionSetsDir` (e.g. via a symlink) are rejected.
2. Files larger than 1 MiB are skipped (logged as a warning).
3. Files whose name starts with `claude-` are bound to Claude; files whose
   name starts with `copilot-` are bound to Copilot. Anything else is
   ignored.
4. The default for each tool is `<tool>-default.md` if it exists, otherwise
   the first alphabetical match for that tool's prefix.
5. The `id` of a discovered set is the filename **without the `.md`
   extension** (e.g. `claude-default`).

## Quarantine: recovering from a bad `config.json`

Grove never crashes on a corrupt config file. If `config.json` (or
`sessions.json`) fails to parse — invalid JSON, schema mismatch, unknown
enum values — Grove:

1. Renames the bad file to `config.json.bad-<unix-timestamp>` (or
   `sessions.json.bad-<unix-timestamp>`) inside the same directory.
2. Logs a `tracing::warn!` event with `code = "ConfigQuarantined"` naming the
   quarantined file.
3. Returns defaults (an empty `AppConfig` for `config.json`, an empty session
   map for `sessions.json`) and continues running.

To recover settings from a quarantined file:

1. Stop Grove.
2. Open the `*.bad-<timestamp>` file in your editor and fix the syntax.
3. Move it back to its original name (`config.json` / `sessions.json`).
4. Start Grove again — the loader will pick it up.

If you don't need the contents, just delete the `*.bad-*` files. They will
not be cleaned up automatically.

## 11. The repo `instructions/` directory vs runtime `instructionSetsDir`

The repository ships starter instruction-set files at
`instructions/claude-default.md` and `instructions/copilot-default.md`.
These are **convenience defaults for developers**, not part of the
runtime contract:

- At launch Grove reads `AppConfig.instructionSetsDir` (an absolute path
  in the user's `config.json`) and discovers `*.md` files under it
  following the rules in §1 (Instruction set discovery).
- The repo's `instructions/` directory is just a starter set you can
  copy somewhere safe and point `instructionSetsDir` at — or, in
  development, point `instructionSetsDir` straight at the repo path.
- Editing files inside the repo `instructions/` directory has no effect
  on a Grove install whose `instructionSetsDir` points elsewhere.

Treat the repo's `instructions/` directory the way you'd treat a
template: copy, adapt, and configure `instructionSetsDir` to wherever
your real instruction sets live.

## Editing safely

- **Stop Grove first.** The backend writes `sessions.json` atomically, but
  hand-edits during a live session may be overwritten by a session
  create/close/restart.
- Validate JSON before saving (most editors do this for you).
- Paths with spaces, backslashes, or other shell metacharacters are fine on
  disk — Grove shell-quotes any value it interpolates into a launch command.
