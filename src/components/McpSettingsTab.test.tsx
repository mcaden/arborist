// Behaviour-level tests for the MCP settings tab. Asserts:
// - The master toggle is off by default and gates the tool grid visibility.
// - Destructive tools cannot have their confirmation mode lowered below
//   their security floor.
// - Saving emits the minimal config-set patch (only changed fields).

import { describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen } from '@testing-library/react';

import { McpSettingsTab } from './McpSettingsTab';
import { useConfigStore } from '@/store/config-store';
import type { AppConfig, AppConfigMcp } from '@/types/arborist';
import { makeDefaultMcpConfig } from '@/types/arborist';
import { appConfigFixture } from '@/types/fixtures/appConfig';
import * as bridge from '@/lib/tauri-bridge';

vi.mock('@/lib/tauri-bridge', async () => await import('@/lib/tauri-bridge.mock'));

function seed(mcpEnabled: boolean): void {
  const baseMcp: AppConfigMcp = { ...makeDefaultMcpConfig(), enabled: mcpEnabled };
  const cfg: AppConfig = { ...structuredClone(appConfigFixture), mcp: baseMcp };
  useConfigStore.setState({ config: cfg, status: 'ready', error: null });
}

describe('McpSettingsTab', () => {
  it('hides the tool grid when MCP is off and reveals it after enabling', () => {
    seed(false);
    render(<McpSettingsTab onClose={() => {}} />);
    expect(screen.queryByTestId('mcp-tools-section')).toBeNull();
    fireEvent.click(screen.getByTestId('mcp-master-toggle'));
    expect(screen.getByTestId('mcp-tools-section')).toBeInTheDocument();
  });

  it('refuses to expose a "never" option for the cleanup tool (security floor)', () => {
    seed(true);
    render(<McpSettingsTab onClose={() => {}} />);
    const select = screen.getByTestId('mcp-tool-cleanup_merged_worktrees-confirm') as HTMLSelectElement;
    const values = Array.from(select.options).map((o) => o.value);
    expect(values).toEqual(['always']);
  });

  it('exposes firstUse and always for the create_worktree tool, but not never', () => {
    seed(true);
    render(<McpSettingsTab onClose={() => {}} />);
    const select = screen.getByTestId('mcp-tool-create_worktree-confirm') as HTMLSelectElement;
    const values = Array.from(select.options).map((o) => o.value);
    expect(values).toEqual(['firstUse', 'always']);
  });

  it('emits a minimal config_set patch with only the changed fields when saving', async () => {
    seed(true);
    const setSpy = vi.spyOn(bridge, 'configSet').mockImplementation(async (patch) => {
      const cfg = useConfigStore.getState().config;
      // Honour the partial-merge contract: only `mcp.enabled` / `allowRemoteFetch`
      // / `tools` change in this test, so we splice them in keeping the fully-typed
      // mcp scaffold from the seed call.
      const mergedTools = { ...cfg.mcp.tools };
      for (const [id, partial] of Object.entries(patch.mcp?.tools ?? {})) {
        const existing = mergedTools[id] ?? { enabled: true, requiresConfirmation: 'never' as const };
        mergedTools[id] = {
          enabled: partial.enabled ?? existing.enabled,
          requiresConfirmation: partial.requiresConfirmation ?? existing.requiresConfirmation,
        };
      }
      const mergedMcp: AppConfigMcp = {
        ...cfg.mcp,
        enabled: patch.mcp?.enabled ?? cfg.mcp.enabled,
        allowRemoteFetch: patch.mcp?.allowRemoteFetch ?? cfg.mcp.allowRemoteFetch,
        tools: mergedTools,
      };
      return { ...cfg, mcp: mergedMcp };
    });
    render(<McpSettingsTab onClose={() => {}} />);
    fireEvent.click(screen.getByTestId('mcp-tool-list_worktrees-enabled'));
    await act(async () => {
      fireEvent.click(screen.getByTestId('mcp-save'));
    });
    expect(setSpy).toHaveBeenCalledTimes(1);
    const patch = setSpy.mock.calls[0]![0];
    expect(patch.mcp?.enabled).toBeUndefined();
    expect(patch.mcp?.allowRemoteFetch).toBeUndefined();
    expect(patch.mcp?.tools).toEqual({ list_worktrees: { enabled: false, requiresConfirmation: 'never' } });
  });
});
