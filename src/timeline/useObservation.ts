import { useEffect } from 'react';
import type { DaemonStatus } from '../bridge';

export function useTimelineObservation(status: DaemonStatus, trackId: string | null, repairRunId: string | null) {
  useEffect(() => {
    if (!status.connected || status.mode === 'preview') return;
    const observe = () => {
      const view = trackId ? { trackId, repairRunId } : null;
      void window.intercat.observeTimeline(view).catch(() => {});
    };
    observe();
    const timer = setInterval(observe, 5000);
    return () => {
      clearInterval(timer);
      void window.intercat.observeTimeline(null).catch(() => {});
    };
  }, [status.connected, status.mode, status.daemonPid, trackId, repairRunId]);
}
