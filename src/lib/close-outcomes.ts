// Shared formatter for `SubSessionCloseResult` → user-facing copy.
//
// Two call sites historically duplicated this branching:
//   * `SubCloseConfirmDialog` — single-sub close, shows a follow-up
//     alert when the close didn't do exactly what the button promised.
//   * `WorktreeCloseConfirmDialog` — cascade close, summarises every
//     sub that needs attention as a bullet list.
//
// Centralising keeps the language consistent across both surfaces and
// makes adding a new `SubSessionCloseStatus` variant a single-file
// change: extend `formatSubCloseOutcome` and both dialogs pick up the
// new copy automatically.

import type { SubSessionCloseResult } from '@/types/arborist';

/**
 * Translate a `SubSessionCloseResult` into a user-facing sentence,
 * or `null` when the result needs no follow-up message (the close
 * did exactly what its label promised).
 *
 * `bullet` — when true, prefixes the message with `• ` so the caller
 * can stitch lines together for a cascade summary. The single-sub
 * call site leaves it false to produce a standalone sentence.
 *
 * `idLabel` — short identifier inserted at the start of bullet rows
 * for cascade summaries so the user can tell sub-tabs apart. Ignored
 * when `bullet` is false.
 */
export function formatSubCloseOutcome(result: SubSessionCloseResult, options?: { bullet?: boolean; idLabel?: string }): string | null {
  const bullet = options?.bullet ?? false;
  const idLabel = options?.idLabel;
  const pidSuffix = result.pid !== undefined ? ` (pid ${result.pid})` : '';
  const sentence = sentenceFor(result, pidSuffix);
  if (sentence === null) return null;
  if (!bullet) return sentence;
  // Cascade summary: `• <id>: <copy>` so users can map back to a specific sub-tab in the sidebar.
  const prefix = idLabel ? `• ${idLabel}: ` : '• ';
  return `${prefix}${sentence}`;
}

/**
 * Build a multi-line bullet summary of every sub-close outcome in
 * `subOutcomes` that needs follow-up. Returns the empty string when
 * every sub closed cleanly so the caller can skip alert noise.
 */
export function summariseSubCloseOutcomes(subOutcomes: Record<string, SubSessionCloseResult> | undefined): string {
  if (!subOutcomes) return '';
  const lines: string[] = [];
  for (const [id, result] of Object.entries(subOutcomes)) {
    // Use a short prefix so cascades over many sub-tabs stay scannable; full id is recoverable from the sidebar status row.
    const idLabel = id.length > 8 ? `${id.slice(0, 8)}…` : id;
    const formatted = formatSubCloseOutcome(result, { bullet: true, idLabel });
    if (formatted !== null) lines.push(formatted);
  }
  return lines.join('\n');
}

function sentenceFor(result: SubSessionCloseResult, pidSuffix: string): string | null {
  switch (result.status) {
    case 'confirmed':
      return null;
    case 'unsupported':
      return `This operating system doesn't support requesting an app close${pidSuffix} — tab detached.`;
    case 'unavailable':
      return `Arborist couldn't identify the exact app window${pidSuffix} — tab detached.`;
    case 'refusedShared':
      return `Refused to terminate a shared editor process${pidSuffix}: killing it would also close your other workspace windows. The tab was detached.`;
    case 'unconfirmed':
      if (result.outcome === 'forceKill') {
        return `Force-kill signal sent${pidSuffix}, but the operating system didn't confirm the process exited within the grace window. The process may still be alive — check Task Manager / Activity Monitor.`;
      }
      if (result.outcome === 'politeClose') {
        return `Asked the app to close${pidSuffix}, but it's still running after the grace window — it may be showing a "Save changes?" prompt. The tab was removed.`;
      }
      return `Close signal sent${pidSuffix}, but the operating system didn't confirm the process exited within the grace window.`;
    default:
      return null;
  }
}
