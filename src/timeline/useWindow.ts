import { useEffect, useRef, useState } from 'react';
import type { DaemonStatus, TimelineClip, TimelineMark } from '../bridge';
export interface TimelineBounds { tracks: string[]; start: number; end: number }

export function useTimelineWindow(bounds: TimelineBounds | null, status: DaemonStatus) {
  const cache = useRef({ key: '', revision: null as number | null, clips: new Map<string, TimelineClip>(), marks: new Map<string, TimelineMark>() });
  const [value, setWindow] = useState<{ clips: TimelineClip[]; marks: TimelineMark[] }>({ clips: [], marks: [] });
  useEffect(() => {
    if (!bounds || !bounds.tracks.length || !status.connected || status.mode === 'preview') return;
    let disposed = false;
    // A held arrow or wheel can cross dozens of rows before the next paint.
    // Fetch the settled viewport instead of serializing every intermediate row.
    const timer = setTimeout(() => {
      const key = JSON.stringify([status.daemonPid, bounds]);
      if (cache.current.key !== key) cache.current = { key, revision: null, clips: new Map(), marks: new Map() };
      const target = cache.current;
      void window.intercat.getTimelineWindow({ ...bounds, since: target.revision }).then(value => {
        if (disposed || target !== cache.current) return;
        target.revision = value.revision;
        if (value.reset) { target.clips.clear(); target.marks.clear(); }
        for (const id of value.removed) target.clips.delete(id);
        for (const clip of value.clips) target.clips.set(clip.id, clip);
        for (const id of value.removedMarks) target.marks.delete(id);
        for (const mark of value.marks) target.marks.set(mark.id, mark);
        if (value.reset || value.clips.length || value.removed.length || value.marks.length || value.removedMarks.length) setWindow({ clips: [...target.clips.values()], marks: [...target.marks.values()] });
      }).catch(() => { /* Keep the last verified window until the next observation. */ });
    }, 40);
    return () => { disposed = true; clearTimeout(timer); };
  }, [bounds, status.connected, status.mode, status.sequence, status.daemonPid]);
  return value;
}
