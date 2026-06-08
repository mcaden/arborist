import { useEffect, useMemo, useRef, useState } from 'react';

import { ToolIcon } from './ToolIcon';
import { measureInitialPtyDimensions } from '@/hooks/use-terminal';
import { formatError } from '@/lib/tauri-bridge';
import { pluginEnabled, useRegistry } from '@/plugins';
import { useConfigStore } from '@/store/config-store';
import { useSessionActions } from '@/store/session-store';
import { useWorktreeTabStore } from '@/store/worktree-tab-store';
import type { Tool, WorktreeTabId } from '@/types/arborist';

interface WorktreeDashboardProps {
  tabId: WorktreeTabId;
}

export function WorktreeDashboard({ tabId }: WorktreeDashboardProps): JSX.Element | null {
  const tab = useWorktreeTabStore((s) => s.tabs.find((t) => t.id === tabId));
  const sessionActions = useSessionActions();
  const registry = useRegistry();
  const pluginSettings = useConfigStore((s) => s.config.pluginSettings);
  const isMountedRef = useRef<boolean>(false);
  useEffect(() => {
    isMountedRef.current = true;
    return () => {
      isMountedRef.current = false;
    };
  }, []);
  const aiPlugins = useMemo(
    () => registry.ai().filter((plugin) => pluginEnabled(pluginSettings, 'ai', plugin.id, plugin.defaultEnabled ?? true)),
    [registry, pluginSettings],
  );
  const aiIconDataUris = useConfigStore((s) => s.config.aiLaunchCommands.iconDataUris);
  const [launchError, setLaunchError] = useState<string | null>(null);
  const widgets = useMemo(
    () => registry.widgets().filter((widget) => pluginEnabled(pluginSettings, 'dashboardWidget', widget.id, widget.defaultEnabled ?? true)),
    [registry, pluginSettings],
  );

  if (!tab) {
    return null;
  }

  const launch = (tool: Tool): void => {
    setLaunchError(null);
    const pluginName = aiPlugins.find((plugin) => plugin.id === tool)?.displayName ?? tool;
    const dims = measureInitialPtyDimensions();
    sessionActions
      .create({
        tool,
        worktreePath: tab.path,
        cols: dims.cols,
        rows: dims.rows,
      })
      .catch((err: unknown) => {
        const message = formatError(err);
        console.warn(`[WorktreeDashboard] sessionCreate(${tool}) failed: ${message}`);
        if (!isMountedRef.current) return;
        setLaunchError(`Launch ${pluginName} failed: ${message}`);
      });
  };

  return (
    <section
      data-testid="worktree-dashboard"
      role="region"
      aria-labelledby="worktree-dashboard-title"
      className="themed-scrollbar flex h-full min-w-0 flex-1 flex-col gap-6 overflow-y-auto bg-white px-8 py-6 text-slate-700 dark:bg-slate-950 dark:text-slate-200"
    >
      <header className="flex flex-col gap-1">
        <h1 id="worktree-dashboard-title" className="text-lg font-semibold">
          {tab.name}
        </h1>
        <p className="font-mono text-xs text-slate-500 dark:text-slate-400">{tab.path}</p>
        {tab.branch && <p className="text-xs text-slate-500 dark:text-slate-400">on branch {tab.branch}</p>}
      </header>

      <div className="flex gap-3">
        {aiPlugins.map((plugin) => {
          const iconDataUri = aiIconDataUris[plugin.id];
          return (
            <button
              key={plugin.id}
              type="button"
              data-testid={`worktree-dashboard-launch-${plugin.id}`}
              onClick={() => launch(plugin.id)}
              className="inline-flex items-center gap-2 rounded-md border border-slate-300 bg-white px-4 py-2 text-sm font-medium text-slate-700 hover:bg-slate-50 focus:outline-none focus-visible:ring-2 focus-visible:ring-sky-500 dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100 dark:hover:bg-slate-800"
            >
              <ToolIcon tool={plugin.id} className="h-4 w-4" {...(iconDataUri ? { iconDataUri } : {})} />
              <span>{`Launch ${plugin.displayName}`}</span>
            </button>
          );
        })}
      </div>
      {launchError && (
        <p
          role="alert"
          data-testid="worktree-dashboard-launch-error"
          className="rounded border border-red-300 bg-red-50 px-3 py-2 text-sm text-red-700 dark:border-red-900/70 dark:bg-red-950/40 dark:text-red-300"
        >
          {launchError}
        </p>
      )}

      <div className="grid gap-4 md:grid-cols-2">
        {widgets.map((widget) => (
          <widget.Component key={widget.id} tabId={tab.id} tabPath={tab.path} />
        ))}
      </div>
    </section>
  );
}
