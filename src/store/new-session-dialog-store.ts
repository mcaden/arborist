// Tiny standalone Zustand store for the "create session" dialog visibility.
// Decoupled from `session-store` so opening/closing the dialog doesn't
// re-render every session-list subscriber, and so `NewSessionButton` can
// stay a leaf component with no extra props (the alternative — lifting the
// open state into `App.tsx` and threading a callback prop down through
// `Sidebar` — would force every test that mounts the sidebar to provide
// the prop). Phase 12 may move this into a higher-level UI store.

import { create } from 'zustand';

interface DialogState {
  isOpen: boolean;
  open: () => void;
  close: () => void;
}

export const useNewSessionDialog = create<DialogState>((set) => ({
  isOpen: false,
  open: () => set({ isOpen: true }),
  close: () => set({ isOpen: false }),
}));
