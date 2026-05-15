import { useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';

import {
  resolveShellCommandTrustRequest,
  subscribeShellCommandTrustRequests,
  type ShellCommandTrustRequest,
  type TrustChoice,
} from '@/lib/shell-command-trust';
import type { ShellCommandPreviewItem } from '@/types/arborist';

function kindLabel(item: ShellCommandPreviewItem): string {
  switch (item.kind) {
    case 'aiLaunch':
      return 'AI launch';
    case 'worktreePrep':
      return 'Worktree prep';
  }
}

function sourceLabel(item: ShellCommandPreviewItem): string {
  return item.sourcePath ?? item.source;
}

function untrustedCommands(request: ShellCommandTrustRequest): ShellCommandPreviewItem[] {
  return request.preview.commands.filter((command) => !command.trusted);
}

export function ShellCommandTrustDialogHost(): JSX.Element | null {
  const [request, setRequest] = useState<ShellCommandTrustRequest | null>(null);
  const headingId = useId();
  const descriptionId = useId();
  const runOnceRef = useRef<HTMLButtonElement | null>(null);
  const previouslyFocusedRef = useRef<HTMLElement | null>(null);

  useEffect(() => subscribeShellCommandTrustRequests(setRequest), []);

  useEffect(() => {
    if (request === null) return;
    previouslyFocusedRef.current = typeof document !== 'undefined' ? (document.activeElement as HTMLElement | null) : null;
    runOnceRef.current?.focus();
    return () => {
      const previous = previouslyFocusedRef.current;
      previouslyFocusedRef.current = null;
      if (previous && typeof previous.focus === 'function' && document.contains(previous)) {
        previous.focus();
      }
    };
  }, [request]);

  useEffect(() => {
    if (request === null) return;
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        resolveShellCommandTrustRequest(request.id, 'cancel');
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  }, [request]);

  if (request === null) {
    return null;
  }

  const commands = untrustedCommands(request);
  const scopeLabel = commands.length === 1 ? 'this exact command' : 'these exact commands';
  const rememberLabel = commands.length === 1 ? "Don't ask again for this exact command" : "Don't ask again for these exact commands";
  const choose = (choice: TrustChoice): void => resolveShellCommandTrustRequest(request.id, choice);

  return createPortal(
    <div
      data-testid="shell-command-trust-backdrop"
      className="fixed inset-0 z-[60] flex items-center justify-center bg-slate-950/60 p-4 backdrop-blur-sm"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          choose('cancel');
        }
      }}
    >
      <section
        role="dialog"
        aria-modal="true"
        aria-labelledby={headingId}
        aria-describedby={descriptionId}
        className="max-h-[90vh] w-full max-w-3xl overflow-hidden rounded-lg border border-slate-300 bg-white text-slate-900 shadow-2xl dark:border-slate-700 dark:bg-slate-900 dark:text-slate-100"
        onMouseDown={(event) => event.stopPropagation()}
      >
        <div className="border-b border-slate-200 px-5 py-4 dark:border-slate-800">
          <h2 id={headingId} className="text-base font-semibold">
            Trust repository command
          </h2>
          <p id={descriptionId} className="mt-2 text-sm text-slate-600 dark:text-slate-300">
            This repository defines executable Arborist settings in <code className="font-mono">.arborist/settings.json</code>. Review them before
            Arborist runs them. The "don't ask again" choice applies only to {scopeLabel}; changes will ask again.
          </p>
        </div>

        <div className="max-h-[55vh] space-y-4 overflow-auto px-5 py-4">
          <p className="text-sm text-slate-700 dark:text-slate-200">
            <span className="font-medium">Target worktree:</span>{' '}
            <code className="break-all rounded bg-slate-100 px-1 py-0.5 font-mono text-xs dark:bg-slate-800">
              {request.preview.targetWorktreePath}
            </code>
          </p>

          {commands.map((item) => (
            <article
              key={`${item.kind}-${item.scope ?? 'global'}-${sourceLabel(item)}-${item.command}`}
              className="rounded-md border border-slate-200 p-3 dark:border-slate-800"
            >
              <dl className="grid gap-2 text-sm sm:grid-cols-[auto_1fr]">
                <dt className="font-medium text-slate-600 dark:text-slate-300">Source</dt>
                <dd className="break-all font-mono text-xs text-slate-800 dark:text-slate-100">{sourceLabel(item)}</dd>
                <dt className="font-medium text-slate-600 dark:text-slate-300">Kind</dt>
                <dd>{kindLabel(item)}</dd>
                {item.scope && (
                  <>
                    <dt className="font-medium text-slate-600 dark:text-slate-300">Scope</dt>
                    <dd>{item.scope}</dd>
                  </>
                )}
                <dt className="font-medium text-slate-600 dark:text-slate-300">Command</dt>
                <dd>
                  <pre className="whitespace-pre-wrap break-words rounded bg-slate-100 p-2 font-mono text-xs text-slate-900 dark:bg-slate-950 dark:text-slate-100">
                    {item.command}
                  </pre>
                </dd>
              </dl>
            </article>
          ))}
        </div>

        <div className="flex flex-wrap justify-end gap-2 border-t border-slate-200 px-5 py-4 dark:border-slate-800">
          <button
            ref={runOnceRef}
            type="button"
            className="rounded border border-slate-300 bg-white px-3 py-2 text-sm hover:bg-slate-100 focus:outline-none focus:ring-2 focus:ring-blue-500 dark:border-slate-700 dark:bg-slate-900 dark:hover:bg-slate-800"
            onClick={() => choose('once')}
          >
            Run once
          </button>
          <button
            type="button"
            className="rounded bg-blue-600 px-3 py-2 text-sm font-medium text-white hover:bg-blue-500 focus:outline-none focus:ring-2 focus:ring-blue-500 focus:ring-offset-2 focus:ring-offset-white dark:focus:ring-offset-slate-900"
            onClick={() => choose('always')}
          >
            {rememberLabel}
          </button>
          <button
            type="button"
            className="rounded border border-slate-300 bg-white px-3 py-2 text-sm hover:bg-slate-100 focus:outline-none focus:ring-2 focus:ring-blue-500 dark:border-slate-700 dark:bg-slate-900 dark:hover:bg-slate-800"
            onClick={() => choose('cancel')}
          >
            Cancel
          </button>
        </div>
      </section>
    </div>,
    document.body,
  );
}
