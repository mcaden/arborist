import { useCallback, useEffect, useRef, useState } from 'react';

import { formatError, worktreePrInfo } from '@/lib/tauri-bridge';
import type { WorktreePrInfo } from '@/types/arborist';

// PR lookups shell out to a provider CLI (network + auth bound), so they poll on a slower cadence than the local `git status` snapshot.
const POLL_INTERVAL_MS = 60_000;

export interface UsePrInfoResult {
  prInfo: WorktreePrInfo | null;
  prError: string | null;
  prLoading: boolean;
  refreshPrInfo: () => Promise<void>;
}

/**
 * Load and periodically refresh the pull/merge request info for a worktree. Mirrors {@link useGitStatus}'s request-id guarding so a slow in-flight
 * lookup can never clobber a newer result, but polls at {@link POLL_INTERVAL_MS} rather than the status cadence.
 */
export function usePrInfo(tabPath: string): UsePrInfoResult {
  const [prInfo, setPrInfo] = useState<WorktreePrInfo | null>(null);
  const [prError, setPrError] = useState<string | null>(null);
  const [prLoading, setPrLoading] = useState(false);
  const reqIdRef = useRef(0);
  const inFlightRef = useRef(false);

  const refreshPrInfo = useCallback(async () => {
    if (!tabPath) return;
    if (inFlightRef.current) return;
    inFlightRef.current = true;
    const reqId = ++reqIdRef.current;
    setPrLoading(true);
    try {
      const result = await worktreePrInfo(tabPath);
      if (reqIdRef.current !== reqId) return;
      setPrInfo(result);
      setPrError(null);
    } catch (err) {
      if (reqIdRef.current !== reqId) return;
      setPrError(formatError(err));
    } finally {
      if (reqIdRef.current === reqId) {
        inFlightRef.current = false;
        setPrLoading(false);
      }
    }
  }, [tabPath]);

  useEffect(() => {
    if (!tabPath) return;
    setPrInfo(null);
    setPrError(null);
    inFlightRef.current = false;
    setPrLoading(false);

    void refreshPrInfo();
    const handle = window.setInterval(() => {
      void refreshPrInfo();
    }, POLL_INTERVAL_MS);
    const refSnapshot = reqIdRef;
    return () => {
      window.clearInterval(handle);
      refSnapshot.current++;
    };
  }, [tabPath, refreshPrInfo]);

  return { prInfo, prError, prLoading, refreshPrInfo };
}
