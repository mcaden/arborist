# MCP server

Arborist ships an embedded [Model Context Protocol](https://modelcontextprotocol.io/) server that lets AI sessions running inside Arborist sessions
inspect and manage Git worktrees through a small, audited tool surface. The server is **off by default** in every workspace; users opt in per
workspace from **Settings → MCP Server**, and individual tools can be disabled or set to prompt before every invocation.

This page is the user-facing reference. For implementation detail (Rust modules, IPC handshake, audit log layout) see [`architecture.md`](./architecture.md#mcp-server)
and the per-tool design docs under `dev/ai/` (agent-only working files).

## What the server exposes

| Tool                        | Effect      | Default confirmation | Notes                                                                                                                                                                                     |
| --------------------------- | ----------- | -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `list_worktrees`            | read-only   | never                | Returns the workspace's worktree inventory (path, branch, HEAD, dirty/locked status). No FS writes.                                                                                       |
| `workspace_status`          | read-only   | never                | Returns workspace bind state, default branch, prep commands, and the per-tool effective config for the bound session.                                                                     |
| `create_worktree`           | destructive | first use            | Creates a new git worktree and optionally launches an AI session in it. First use in a session pops a confirmation prompt; subsequent calls within that session reuse the trust grant.    |
| `merge_main_into_worktrees` | destructive | always               | Fast-forwards / merges the default branch into the selected worktrees. Always prompts the user; refuses dirty trees and trees with live sessions unless explicitly opted in.              |
| `cleanup_merged_worktrees`  | destructive | always               | Removes worktrees whose branches are fully merged into the default branch. Always prompts; refuses to touch dirty trees, the user's own worktree, or trees that still own a live session. |

Every tool runs inside the workspace bound to the calling session — there is no way for a tool call to read or mutate state outside that workspace
root.

## Security posture

**Threat model summary.** The MCP server's adversary is a _compromised or malicious AI model_ whose tool-call outputs reach Arborist over an
authenticated local socket. The defences below are layered so any one of them being bypassed still leaves the others standing.

| Defence                                  | What it prevents                                                                                                                                                                                                                   |
| ---------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Off by default**                       | A user who never opts in is never exposed. Disabling the workspace toggle tears the host IPC server down immediately.                                                                                                              |
| **OS-authenticated socket**              | The host IPC listener binds to a local-only socket whose peer is verified by the OS (Unix peer creds / Windows named-pipe SID). No remote attacker can reach it.                                                                   |
| **Per-session binding**                  | Every IPC connection is bound to exactly one Arborist session id and one workspace root. A tool call can only see and act on that session's worktree.                                                                              |
| **Capability allow-list**                | The host only registers five tools (the ones above). New tools require a code change _and_ a workspace-config enable; there is no dynamic-tool discovery path.                                                                     |
| **Confirmation gating**                  | Destructive tools default to `firstUse` or `always` confirmation. The dropdown in Settings cannot lower a destructive tool below its safe floor.                                                                                   |
| **Confirmation token replay protection** | Approvals mint a short-lived single-use token bound to the _exact_ arguments. If the worktree drifts between approval and execution (e.g. a session is restarted or files change), the token is rejected with `ConfirmationStale`. |
| **Rate limits**                          | Per-session / per-workspace / per-host token buckets cap structural reads, expensive reads, destructive ops, and remote fetches. Limits are non-configurable in the UI to keep them as a security floor.                           |
| **Tamper-evident audit log**             | Every read and every destructive operation is appended to a hash-chained audit log. Read and destructive logs are separate files; the chain detects retroactive edits.                                                             |
| **Refuse the user's own worktree**       | `cleanup_merged_worktrees` and `merge_main_into_worktrees` will not act on the worktree that the calling session itself lives in.                                                                                                  |
| **No remote fetch by default switch**    | Users can disable the network-touching code path (`git fetch --no-tags --quiet origin`). With it off, cleanup falls back to the merge state already on disk.                                                                       |

The audit log lives next to the workspace config (see [`configuration.md`](./configuration.md)) and is readable from **Settings → MCP Server →
Audit log** once the viewer ships.

## Settings UI

Open **Settings → MCP Server** to:

1. Toggle the workspace-level MCP master switch (off by default).
2. Disable individual tools or tighten their confirmation mode (you can only raise security, never lower it below the floor).
3. Toggle whether MCP tools may run read-only `git fetch` against `origin` (defaults to on; turning it off keeps MCP fully offline).

Per-session overrides (`AppConfigMcp.perSession`) and granular rate-limit tuning are intentionally **not** in the UI for v1:

- Per-session overrides ship from each session's row UI in a follow-up; until then they can be edited via `config_set` for advanced users.
- Rate limits remain JSON-only — the defaults are the security floor, not a tuning surface.

## What is _not_ in v1

The following items from the original UX wireframe are intentionally deferred to follow-up PRs. None of them weaken the security posture above —
the affected flows fall back to the Settings panel + confirmation dialogs that ship in v1.

- Inline confirmation banner on the requesting session's terminal tab. v1 surfaces confirmations through the standard Tauri dialog the
  `mcp_pending_actions` / `mcp_approve` commands feed.
- Global MCP activity drawer with expired-pending re-request UX.
- Sidebar attention dot for sessions with pending MCP requests.
- Rate-limit current-spend visualisation.
- Audit log viewer in-app (the log is still written and verifiable on disk).
- First-run disclosure modal (the Settings panel copy doubles as the explanation for now).
- Per-session override UI (`perSession`).

These will land as targeted PRs once the v1 surface bakes in production. Open an issue if any of them is a blocker for your use.
