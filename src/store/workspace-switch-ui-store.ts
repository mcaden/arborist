// Pure UI state for the in-app workspace switch. Kept separate from
// `config-store` because it is **not** persisted, doesn't round-trip to
// the backend, and exists solely to gate UI affordances while the
// backend's transactional switch is in flight.
//
// Set to `true` synchronously by `lib/workspace-switch.ts::changeWorkspace`
// before invoking the backend, and cleared in `finally` (after the atomic
// adoption has landed in the other stores). The single render at the
// flag's transition lands the new workspace's data + the flag-off
// together, so the user never sees a "no workspace" flash.
//
// Consumers:
//   * `App.tsx` overlays a "Switching workspace…" panel + sets
//     `aria-busy` + `inert` on the underlying root.
//   * `TerminalView` skips its post-tab-switch `term.focus()` while the
//     flag is true so focus doesn't fight the overlay.

import { create } from 'zustand';

export interface WorkspaceSwitchUiStore {
  isSwitching: boolean;
  setSwitching: (value: boolean) => void;
}

export const useWorkspaceSwitchUiStore = create<WorkspaceSwitchUiStore>((set) => ({
  isSwitching: false,
  setSwitching: (value) => set({ isSwitching: value }),
}));

export const selectIsSwitching = (s: WorkspaceSwitchUiStore): boolean => s.isSwitching;
