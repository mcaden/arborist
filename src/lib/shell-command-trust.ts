import { repoCommandAllowOnce, repoCommandTrust, shellCommandPreview } from '@/lib/tauri-bridge';
import { useConfigStore } from '@/store/config-store';
import type { ShellCommandIntent, ShellCommandPreview } from '@/types/arborist';

export type TrustChoice = 'once' | 'always' | 'cancel';

export interface ShellCommandTrustRequest {
  id: number;
  preview: ShellCommandPreview;
}

type PendingRequest = ShellCommandTrustRequest & {
  resolve: (choice: TrustChoice) => void;
};

type PromptSubscriber = (request: ShellCommandTrustRequest | null) => void;
type PromptAdapter = (preview: ShellCommandPreview) => Promise<TrustChoice>;

let nextRequestId = 1;
let activeRequest: PendingRequest | null = null;
let queuedRequests: PendingRequest[] = [];
let subscriber: PromptSubscriber | null = null;
let promptAdapterForTest: PromptAdapter | null = null;

function notifySubscriber(): void {
  subscriber?.(activeRequest ? { id: activeRequest.id, preview: activeRequest.preview } : null);
}

function pumpQueue(): void {
  if (activeRequest === null) {
    activeRequest = queuedRequests.shift() ?? null;
  }
  notifySubscriber();
}

function cancelPendingRequests(): void {
  const pending = [...(activeRequest ? [activeRequest] : []), ...queuedRequests];
  const hadPending = pending.length > 0;
  activeRequest = null;
  queuedRequests = [];
  if (hadPending) {
    notifySubscriber();
  }
  for (const request of pending) {
    request.resolve('cancel');
  }
}

export function subscribeShellCommandTrustRequests(nextSubscriber: PromptSubscriber): () => void {
  if (subscriber !== null && subscriber !== nextSubscriber) {
    throw new Error('ShellCommandTrustDialogHost is already mounted.');
  }
  subscriber = nextSubscriber;
  pumpQueue();
  return () => {
    if (subscriber === nextSubscriber) {
      subscriber = null;
      cancelPendingRequests();
    }
  };
}

export function requestShellCommandTrustChoice(preview: ShellCommandPreview): Promise<TrustChoice> {
  return new Promise((resolve) => {
    queuedRequests.push({ id: nextRequestId++, preview, resolve });
    pumpQueue();
  });
}

export function resolveShellCommandTrustRequest(id: number, choice: TrustChoice): void {
  if (activeRequest?.id !== id) {
    return;
  }
  const request = activeRequest;
  activeRequest = null;
  request.resolve(choice);
  pumpQueue();
}

export function setShellCommandTrustPromptAdapterForTest(adapter: PromptAdapter | null): () => void {
  const previous = promptAdapterForTest;
  promptAdapterForTest = adapter;
  return () => {
    promptAdapterForTest = previous;
  };
}

export function resetShellCommandTrustPromptStateForTest(): void {
  promptAdapterForTest = null;
  cancelPendingRequests();
  subscriber = null;
}

async function promptTrustChoice(preview: ShellCommandPreview): Promise<TrustChoice> {
  if (promptAdapterForTest !== null) {
    return promptAdapterForTest(preview);
  }
  return requestShellCommandTrustChoice(preview);
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
