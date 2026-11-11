import { useEffect, useState } from 'react';
import type { BranchPoint } from '../bridge';

/** Read only the selected lane's visible interval; a late page cannot replace a
 * different lane or time window. Checkpoint changes invalidate that interval. */
export function useBranchPoints(trackId: string | null, start: number, end: number, checkpoint: string | null, connected: boolean) {
  const [state, setState] = useState({ trackId: '', start: 0, end: 0, points: [] as BranchPoint[], error: '' });
  useEffect(() => {
    if (!connected || !trackId) return;
    let disposed = false;
    void (async () => {
      const points: BranchPoint[] = [];
      const cursors = new Set<number>();
      let before: number | null = null;
      do {
        const page = await window.intercat.getBranchPoints({ trackId, before, range: { start, end } });
        if (disposed) return;
        points.push(...page.points);
        before = page.before;
        if (before !== null) {
          if (cursors.has(before)) throw new Error('Saved branch points could not be paged.');
          cursors.add(before);
        }
      } while (before !== null);
      setState({ trackId, start, end, points, error: '' });
    })().catch(error => { if (!disposed) setState({ trackId, start, end, points: [], error: error instanceof Error ? error.message : 'Saved branch points are unavailable.' }); });
    return () => { disposed = true; };
  }, [trackId, start, end, checkpoint, connected]);
  return connected && state.trackId === trackId && state.start === start && state.end === end ? state : { points: [], error: '' };
}
