// Shared "switch the active workspace" flow used by both the sidebar
// header (`WorkspaceIndicator`) and the in-app Settings panel
// (`SettingsDialog`). Centralised here so the close-all-sessions
// invariant (Roadmap §1.3) is enforced in exactly one place.
//
// Behaviour: close every open session in order. If any close fails,
// throw without touching `config.workspaceRoot` so the caller can keep
// the user on the picker. On full success, persist the new root via
// `config.set`.

import { useConfigStore } from '@/store/config-store';
import { useSessionStore } from '@/store/session-store';

export async function changeWorkspace(path: string): Promise<void> {
  const { sessions, actions } = useSessionStore.getState();
  const failures: string[] = [];
  for (const session of sessions) {
    try {
      await actions.close(session.id);
    } catch (err) {
      failures.push(session.label);
      console.error('Failed to close session before workspace switch', session.id, err);
    }
  }
  if (failures.length > 0) {
    const word = failures.length === 1 ? 'session' : 'sessions';
    throw new Error(
      `Could not close ${failures.length} ${word}: ${failures.join(', ')}. Resolve and try again.`,
    );
  }
  await useConfigStore.getState().set({ workspaceRoot: path });
}
