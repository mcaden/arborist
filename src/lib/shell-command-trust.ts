import { repoCommandAllowOnce, repoCommandTrust, shellCommandPreview } from '@/lib/tauri-bridge';
import { useConfigStore } from '@/store/config-store';
import type { ShellCommandIntent, ShellCommandPreview } from '@/types/arborist';

type TrustChoice = 'once' | 'always' | 'cancel';

function formatTrustPrompt(preview: ShellCommandPreview): string {
  const untrusted = preview.commands.filter((command) => !command.trusted);
  const scopeLabel = untrusted.length === 1 ? 'this exact command' : 'these exact commands';
  const lines = [
    'This repository defines executable Arborist settings in .arborist/settings.json.',
    'Review and trust them before Arborist runs them.',
    `The "don't ask again" choice applies only to ${scopeLabel}; changes will ask again.`,
    '',
    `Target worktree: ${preview.targetWorktreePath}`,
    '',
  ];

  for (const item of untrusted) {
    lines.push(`Source: ${item.sourcePath ?? item.source}`);
    lines.push(`Kind: ${item.kind}`);
    if (item.scope) {
      lines.push(`Scope: ${item.scope}`);
    }
    lines.push('Command:');
    lines.push(item.command);
    lines.push('');
  }

  lines.push('Choose whether to run once, remember this exact command, or cancel.');
  return lines.join('\n');
}

function applyDialogStyle(el: HTMLElement, styles: Partial<CSSStyleDeclaration>): void {
  Object.assign(el.style, styles);
}

function makeButton(label: string, choice: TrustChoice, choose: (choice: TrustChoice) => void): HTMLButtonElement {
  const button = document.createElement('button');
  button.type = 'button';
  button.textContent = label;
  button.addEventListener('click', () => choose(choice));
  applyDialogStyle(button, {
    border: '1px solid #6b7280',
    borderRadius: '6px',
    padding: '6px 10px',
  });
  return button;
}

function promptTrustChoice(preview: ShellCommandPreview): Promise<TrustChoice> {
  return new Promise((resolve) => {
    const overlay = document.createElement('div');
    overlay.setAttribute('role', 'presentation');
    applyDialogStyle(overlay, {
      position: 'fixed',
      inset: '0',
      zIndex: '2147483647',
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      background: 'rgba(0, 0, 0, 0.55)',
    });

    const dialog = document.createElement('div');
    dialog.setAttribute('role', 'dialog');
    dialog.setAttribute('aria-modal', 'true');
    dialog.setAttribute('aria-label', 'Trust repository command');
    applyDialogStyle(dialog, {
      maxWidth: '720px',
      width: 'calc(100% - 32px)',
      color: '#111827',
      background: '#ffffff',
      borderRadius: '10px',
      boxShadow: '0 20px 40px rgba(0, 0, 0, 0.35)',
      padding: '16px',
    });

    const body = document.createElement('pre');
    body.textContent = formatTrustPrompt(preview);
    applyDialogStyle(body, {
      margin: '0 0 16px',
      maxHeight: '50vh',
      overflow: 'auto',
      whiteSpace: 'pre-wrap',
      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
      fontSize: '12px',
    });

    const buttons = document.createElement('div');
    applyDialogStyle(buttons, {
      display: 'flex',
      justifyContent: 'flex-end',
      gap: '8px',
      flexWrap: 'wrap',
    });

    const choose = (choice: TrustChoice): void => {
      document.removeEventListener('keydown', onKeyDown);
      overlay.remove();
      resolve(choice);
    };
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') {
        choose('cancel');
      }
    };

    const untrustedCount = preview.commands.filter((command) => !command.trusted).length;
    const rememberLabel = untrustedCount === 1 ? "Don't ask again for this exact command" : "Don't ask again for these exact commands";
    const runOnce = makeButton('Run once', 'once', choose);
    const trustAlways = makeButton(rememberLabel, 'always', choose);
    const cancel = makeButton('Cancel', 'cancel', choose);
    buttons.append(runOnce, trustAlways, cancel);
    dialog.append(body, buttons);
    overlay.append(dialog);
    document.addEventListener('keydown', onKeyDown);
    document.body.append(overlay);
    runOnce.focus();
  });
}

export async function ensureShellCommandTrusted(intent: ShellCommandIntent): Promise<boolean> {
  const preview = await shellCommandPreview({ intent });
  if (!preview.trustRequired) {
    return true;
  }

  const choice = await promptTrustChoice(preview);
  if (choice === 'cancel') {
    return false;
  }

  if (choice === 'always') {
    const config = await repoCommandTrust({ intent });
    useConfigStore.setState({ config, status: 'ready', error: null });
  } else {
    await repoCommandAllowOnce({ intent });
  }
  return true;
}
