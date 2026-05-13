import { useCallback, useEffect, useRef, useState } from 'react';

import { formatError, worktreeGitStatus } from '@/lib/tauri-bridge';
import type { WorktreeGitStatus } from '@/types/arborist';

const POLL_INTERVAL_MS = 15_000;

export interface UseGitStatusResult {
  status: WorktreeGitStatus | null;
  statusError: string | null;
  statusLoading: boolean;
  refreshStatus: () => Promise<void>;
}

export function useGitStatus(tabPath: string): UseGitStatusResult {
  const [status, setStatus] = useState<WorktreeGitStatus | null>(null);
  const [statusError, setStatusError] = useState<string | null>(null);
  const [statusLoading, setStatusLoading] = useState(false);
  const reqIdRef = useRef(0);
  const inFlightRef = useRef(false);

  const refreshStatus = useCallback(async () => {
    if (!tabPath) return;
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    const reqId = ++reqIdRef.current;
    setStatusLoading(true);
    try {
      const result = await worktreeGitStatus(tabPath);
      if (reqIdRef.current !== reqId) return;
      setStatus(result);
      setStatusError(null);
    } catch (err) {
      if (reqIdRef.current !== reqId) return;
      setStatusError(formatError(err));
    } finally {
      if (reqIdRef.current === reqId) {
        inFlightRef.current = false;
        setStatusLoading(false);
      }
    }
  }, [tabPath]);

  useEffect(() => {
    if (!tabPath) return;
    setStatus(null);
    setStatusError(null);
    inFlightRef.current = false;
    setStatusLoading(false);

    void refreshStatus();
    const handle = window.setInterval(() => {
      void refreshStatus();
    }, POLL_INTERVAL_MS);
    const refSnapshot = reqIdRef;
    return () => {
      window.clearInterval(handle);
      refSnapshot.current++;
    };
  }, [tabPath, refreshStatus]);

  return { status, statusError, statusLoading, refreshStatus };
}
