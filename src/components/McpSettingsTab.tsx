// MCP server settings panel. Owns the *workspace-level* surface area:
// master enable toggle, per-tool enable + confirmation mode, and the
// "allow remote fetch" knob. Per-session overrides (`AppConfigMcp.perSession`)
// and the granular rate-limit grid are intentionally NOT exposed here —
// per-session overrides ship from each session's row UI in a follow-up,
// and rate limits remain JSON-only until we have a real reason to expose
// them (the defaults are the security floor, not a tuning surface).
//
// Security framing for this panel:
// - The master switch defaults to OFF. Users have to opt in *per workspace*
//   before any tool can be invoked at all.
// - Destructive tools (`create_worktree`, `merge_main_into_worktrees`,
//   `cleanup_merged_worktrees`) ship with `requiresConfirmation: 'always'`
//   or `'firstUse'`. The dropdown lets users tighten that to `'always'` but
//   we never auto-loosen it on their behalf.
// - Disabling the master switch tears the host IPC server down via the
//   `mcp_set_enabled` command; this panel writes the config-store patch
//   and the backend reacts to the change.

import { useCallback, useMemo, useState } from 'react';

import { formatError } from '@/lib/tauri-bridge';
import { selectConfig, useConfigStore } from '@/store/config-store';
import type { AppConfigMcp, McpConfirmationMode, McpToolConfig, PartialAppConfigMcp, PartialMcpToolConfig } from '@/types/arborist';

export interface McpSettingsTabProps {
  onClose: () => void;
}

interface ToolRow {
  id: string;
  label: string;
  summary: string;
  destructive: boolean;
  /** Lowest acceptable confirmation mode for security. */
  floor: McpConfirmationMode;
}

// Mirrors the five built-in tools registered by `src-tauri/src/mcp/tools/*`.
// `floor` constrains the per-tool dropdown so users can't accidentally
// downgrade a destructive tool below the safe default.
const TOOL_ROWS: readonly ToolRow[] = [
  {
    id: 'list_worktrees',
    label: 'List worktrees',
    summary: 'Read-only inventory of git worktrees in the workspace, including HEAD, branch, and dirty/locked status.',
    destructive: false,
    floor: 'never',
  },
  {
    id: 'workspace_status',
    label: 'Workspace status',
    summary: 'Read-only snapshot of workspace bind state, default branch, prep commands, and per-tool effective config.',
    destructive: false,
    floor: 'never',
  },
  {
    id: 'create_worktree',
    label: 'Create worktree',
    summary: 'Create a new git worktree (and optionally start a session in it). First use prompts for confirmation.',
    destructive: true,
    floor: 'firstUse',
  },
  {
    id: 'merge_main_into_worktrees',
    label: 'Merge main into worktrees',
    summary: 'Fast-forward / merge the default branch into selected worktrees. Always prompts before touching working trees.',
    destructive: true,
    floor: 'always',
  },
  {
    id: 'cleanup_merged_worktrees',
    label: 'Cleanup merged worktrees',
    summary: 'Remove worktrees whose branches are fully merged into the default branch. Always prompts; refuses dirty/live trees.',
    destructive: true,
    floor: 'always',
  },
];

const CONFIRMATION_OPTIONS: ReadonlyArray<{ value: McpConfirmationMode; label: string }> = [
  { value: 'never', label: 'Never — run without prompting' },
  { value: 'firstUse', label: 'First use — prompt once per session, then remember' },
  { value: 'always', label: 'Always — prompt every invocation' },
];

// The dropdown only offers values at or above the floor (i.e. stricter than
// the security default). We never expose a control that downgrades safety.
function optionsForRow(row: ToolRow): typeof CONFIRMATION_OPTIONS {
  const order: Record<McpConfirmationMode, number> = { never: 0, firstUse: 1, always: 2 };
  return CONFIRMATION_OPTIONS.filter((opt) => order[opt.value] >= order[row.floor]);
}

interface ToolDraft {
  enabled: boolean;
  requiresConfirmation: McpConfirmationMode;
}

interface McpDraft {
  enabled: boolean;
  allowRemoteFetch: boolean;
  tools: Record<string, ToolDraft>;
}

function draftFromConfig(mcp: AppConfigMcp): McpDraft {
  const tools: Record<string, ToolDraft> = {};
  for (const row of TOOL_ROWS) {
    const cfg: McpToolConfig | undefined = mcp.tools[row.id];
    tools[row.id] = {
      enabled: cfg?.enabled ?? true,
      requiresConfirmation: cfg?.requiresConfirmation ?? row.floor,
    };
  }
  return {
    enabled: mcp.enabled,
    allowRemoteFetch: mcp.allowRemoteFetch,
    tools,
  };
}

function draftsEqual(a: McpDraft, b: McpDraft): boolean {
  if (a.enabled !== b.enabled) return false;
  if (a.allowRemoteFetch !== b.allowRemoteFetch) return false;
  const ids = new Set([...Object.keys(a.tools), ...Object.keys(b.tools)]);
  for (const id of ids) {
    const ta = a.tools[id];
    const tb = b.tools[id];
    if (!ta || !tb) return false;
    if (ta.enabled !== tb.enabled) return false;
    if (ta.requiresConfirmation !== tb.requiresConfirmation) return false;
  }
  return true;
}

// Build the minimal PartialAppConfigMcp patch — only fields that actually
// changed. Keeps the on-disk diff small and avoids racing other writers
// that touched unrelated fields.
function buildPatch(draft: McpDraft, persisted: McpDraft): PartialAppConfigMcp | null {
  const patch: PartialAppConfigMcp = {};
  if (draft.enabled !== persisted.enabled) patch.enabled = draft.enabled;
  if (draft.allowRemoteFetch !== persisted.allowRemoteFetch) patch.allowRemoteFetch = draft.allowRemoteFetch;
  const tools: Record<string, PartialMcpToolConfig> = {};
  for (const id of Object.keys(draft.tools)) {
    const cur = draft.tools[id]!;
    const prev = persisted.tools[id];
    if (!prev || cur.enabled !== prev.enabled || cur.requiresConfirmation !== prev.requiresConfirmation) {
      tools[id] = { enabled: cur.enabled, requiresConfirmation: cur.requiresConfirmation };
    }
  }
  if (Object.keys(tools).length > 0) patch.tools = tools;
  return Object.keys(patch).length > 0 ? patch : null;
}

export function McpSettingsTab({ onClose }: McpSettingsTabProps): JSX.Element {
  const config = useConfigStore(selectConfig);
  const setConfig = useConfigStore((s) => s.set);

  const persistedDraft = useMemo(() => draftFromConfig(config.mcp), [config.mcp]);
  const [draft, setDraft] = useState<McpDraft>(persistedDraft);
  // Re-seed local draft if the persisted config changes underfoot
  // (e.g. another window writes, or the workspace switch rehydrates).
  // Compare by value, not reference, so equal snapshots don't blow away
  // in-progress edits.
  const [snapshotKey, setSnapshotKey] = useState<string>(() => JSON.stringify(persistedDraft));
  const persistedKey = JSON.stringify(persistedDraft);
  if (persistedKey !== snapshotKey) {
    setSnapshotKey(persistedKey);
    setDraft(persistedDraft);
  }

  const [saving, setSaving] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);

  const dirty = !draftsEqual(draft, persistedDraft);
  // Hide tool controls when the master switch is off — they're inert at
  // the backend anyway, and showing them implies they take effect.
  const showToolGrid = draft.enabled;

  const handleMasterToggle = useCallback((next: boolean) => {
    setDraft((d) => ({ ...d, enabled: next }));
  }, []);

  const handleAllowRemoteFetch = useCallback((next: boolean) => {
    setDraft((d) => ({ ...d, allowRemoteFetch: next }));
  }, []);

  const handleToolEnabled = useCallback((id: string, next: boolean) => {
    setDraft((d) => ({ ...d, tools: { ...d.tools, [id]: { ...d.tools[id]!, enabled: next } } }));
  }, []);

  const handleToolConfirmation = useCallback((id: string, next: McpConfirmationMode) => {
    setDraft((d) => ({ ...d, tools: { ...d.tools, [id]: { ...d.tools[id]!, requiresConfirmation: next } } }));
  }, []);

  const handleSave = useCallback(async () => {
    setSubmitError(null);
    const patch = buildPatch(draft, persistedDraft);
    if (!patch) {
      onClose();
      return;
    }
    setSaving(true);
    try {
      await setConfig({ mcp: patch });
      onClose();
    } catch (err) {
      setSubmitError(formatError(err));
    } finally {
      setSaving(false);
    }
  }, [draft, persistedDraft, setConfig, onClose]);

  return (
    <section className="space-y-4 text-xs leading-5 text-slate-700 dark:text-slate-200" data-testid="settings-panel-mcp-content">
      <div className="rounded border border-slate-200 p-3 dark:border-slate-700">
        <label className="flex items-start gap-2">
          <input
            type="checkbox"
            checked={draft.enabled}
            onChange={(e) => handleMasterToggle(e.target.checked)}
            data-testid="mcp-master-toggle"
            className="mt-0.5"
          />
          <div className="flex-1">
            <div className="font-medium text-slate-800 dark:text-slate-100">Enable MCP server for this workspace</div>
            <p className="mt-1 text-slate-600 dark:text-slate-300">
              When enabled, Arborist runs a local-only Model Context Protocol server that lets AI sessions in this workspace inspect and manage
              worktrees through audited, rate-limited tools. The server binds to an OS-authenticated local socket only — it is never reachable from
              other machines. Disabled by default; you can turn it off at any time and the host IPC server is torn down immediately.
            </p>
          </div>
        </label>
      </div>

      {showToolGrid ? (
        <>
          <div className="rounded border border-slate-200 p-3 dark:border-slate-700" data-testid="mcp-tools-section">
            <h3 className="mb-2 text-xs font-semibold uppercase tracking-wide text-slate-500 dark:text-slate-400">Tools</h3>
            <p className="mb-3 text-slate-600 dark:text-slate-300">
              Each tool can be disabled individually. Confirmation mode controls whether the user is prompted before the tool runs; destructive tools
              cannot be set lower than their safe default.
            </p>
            <ul className="space-y-3">
              {TOOL_ROWS.map((row) => {
                const tool = draft.tools[row.id]!;
                const opts = optionsForRow(row);
                const confirmId = `mcp-tool-${row.id}-confirm`;
                return (
                  <li key={row.id} className="rounded border border-slate-100 p-2 dark:border-slate-800" data-testid={`mcp-tool-${row.id}`}>
                    <label className="flex items-start gap-2">
                      <input
                        type="checkbox"
                        checked={tool.enabled}
                        onChange={(e) => handleToolEnabled(row.id, e.target.checked)}
                        data-testid={`mcp-tool-${row.id}-enabled`}
                        className="mt-0.5"
                      />
                      <div className="flex-1">
                        <div className="flex items-center gap-2">
                          <span className="font-medium text-slate-800 dark:text-slate-100">{row.label}</span>
                          {row.destructive ? (
                            <span
                              className="rounded bg-amber-100 px-1.5 py-0.5 text-[10px] font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-300"
                              title="This tool can modify the workspace"
                            >
                              destructive
                            </span>
                          ) : null}
                        </div>
                        <p className="mt-0.5 text-slate-600 dark:text-slate-300">{row.summary}</p>
                        <div className="mt-2 flex items-center gap-2">
                          <label htmlFor={confirmId} className="text-slate-500 dark:text-slate-400">
                            Confirmation:
                          </label>
                          <select
                            id={confirmId}
                            value={tool.requiresConfirmation}
                            onChange={(e) => handleToolConfirmation(row.id, e.target.value as McpConfirmationMode)}
                            disabled={!tool.enabled}
                            data-testid={`mcp-tool-${row.id}-confirm`}
                            className="rounded border border-slate-300 bg-white px-2 py-0.5 text-xs disabled:opacity-50 dark:border-slate-600 dark:bg-slate-800"
                          >
                            {opts.map((opt) => (
                              <option key={opt.value} value={opt.value}>
                                {opt.label}
                              </option>
                            ))}
                          </select>
                        </div>
                      </div>
                    </label>
                  </li>
                );
              })}
            </ul>
          </div>

          <div className="rounded border border-slate-200 p-3 dark:border-slate-700">
            <label className="flex items-start gap-2">
              <input
                type="checkbox"
                checked={draft.allowRemoteFetch}
                onChange={(e) => handleAllowRemoteFetch(e.target.checked)}
                data-testid="mcp-allow-remote-fetch"
                className="mt-0.5"
              />
              <div className="flex-1">
                <div className="font-medium text-slate-800 dark:text-slate-100">Allow MCP tools to fetch from remote</div>
                <p className="mt-1 text-slate-600 dark:text-slate-300">
                  Permits read-only network operations (e.g. <code className="font-mono">git fetch --no-tags --quiet origin</code>) when computing
                  merge eligibility for the cleanup and merge tools. Disable to keep MCP fully offline; the cleanup tool will then fall back to
                  whatever merge state is already on disk.
                </p>
              </div>
            </label>
          </div>
        </>
      ) : (
        <p className="rounded border border-dashed border-slate-300 p-3 text-slate-500 dark:border-slate-700 dark:text-slate-400">
          MCP server is off. Enable it above to configure individual tools.
        </p>
      )}

      {submitError ? (
        <p className="rounded border border-rose-300 bg-rose-50 px-3 py-2 text-rose-700 dark:border-rose-700 dark:bg-rose-950/50 dark:text-rose-300">
          {submitError}
        </p>
      ) : null}

      <div className="flex items-center justify-end gap-2 pt-2">
        <button
          type="button"
          onClick={onClose}
          className="rounded border border-slate-300 px-3 py-1 text-xs text-slate-700 hover:bg-slate-100 dark:border-slate-600 dark:text-slate-200 dark:hover:bg-slate-800"
        >
          Cancel
        </button>
        <button
          type="button"
          onClick={handleSave}
          disabled={!dirty || saving}
          data-testid="mcp-save"
          className="rounded bg-blue-600 px-3 py-1 text-xs font-medium text-white hover:bg-blue-700 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </section>
  );
}
