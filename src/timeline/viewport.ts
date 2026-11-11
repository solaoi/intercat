import { REPAIR_ROW_HEIGHT, ROW_HEIGHT, trackRowHeight, type Clip, type Track } from './model';

export interface Lane { key: string; track: Track; repairId: string | null; top: number; height: number; index: number; clips: Clip[]; status: Track['status']; canStop: boolean }
export function lanes(tracks: Track[], overview = false): Lane[] {
  const result: Lane[] = [];
  let top = 0;
  for (const track of tracks) {
    const height = overview ? 12 : trackRowHeight(track);
    result.push({ key: `track-${track.id}`, track, repairId: null, top, height, index: result.length, clips: track.clips, status: track.status, canStop: false });
    top += height;
    for (const repair of track.repairs) {
      const height = overview ? 12 : REPAIR_ROW_HEIGHT;
      result.push({ key: `repair-${repair.id}`, track, repairId: repair.id, top, height, index: result.length, clips: repair.clips, status: repair.status, canStop: repair.canStop });
      top += height;
    }
  }
  return result;
}
export function visibleLanes(rows: Lane[], scroll: number, height: number, overscan = ROW_HEIGHT): Lane[] {
  let lo = 0, hi = rows.length;
  const from = Math.max(0, scroll - overscan), until = scroll + height + overscan;
  while (lo < hi) { const mid = (lo + hi) >>> 1; if (rows[mid].top + rows[mid].height <= from) lo = mid + 1; else hi = mid; }
  const first = lo;
  while (lo < rows.length && rows[lo].top < until) lo++;
  return rows.slice(first, lo);
}
export function revealLane(scroll: number, height: number, row: Lane): number {
  if (row.top < scroll) return row.top;
  if (row.top + row.height > scroll + height) return Math.max(0, row.top + row.height - height);
  return scroll;
}
export function totalHeight(rows: Lane[]): number {
  const last = rows.at(-1);
  return last ? last.top + last.height : 0;
}
