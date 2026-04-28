// App shell. Phase 11 mounts the real <MainArea />.

import { MainArea } from '@/components/MainArea';
import { NewSessionDialog } from '@/components/NewSessionDialog';
import { Sidebar } from '@/components/Sidebar';

export function App(): JSX.Element {
  return (
    <div className="flex h-full w-full bg-white text-slate-900 dark:bg-slate-900 dark:text-slate-100">
      <Sidebar />
      <MainArea />
      <NewSessionDialog />
    </div>
  );
}
