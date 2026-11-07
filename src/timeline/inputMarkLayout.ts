import type { TimelineMark } from '../bridge';
import { geometry, LABEL_WIDTH, type TimelineView } from './model';

export interface MarkGroup { id: string; x: number; marks: TimelineMark[] }

export function markLabel(mark: TimelineMark, tracks: { id: string; label: string }[]) {
  const target = tracks.find(track => track.id === mark.trackId)?.label ?? '?';
  switch (mark.operation) {
    case 'fork': return 'new track';
    case 'trim': return `redirect ${target}`;
    case 'park': return 'parked';
    case 'ask': return 'asked';
    case 'plan': return 'planned';
    case 'schedule': return `after ${target}`;
    case 'command': return mark.command ?? 'application action';
    default: return mark.trackId ? `to ${target}${mark.repairRunId ? ' · for the commit' : ''}` : 'not placed';
  }
}

export function groupMarks(marks: TimelineMark[], view: TimelineView, width: number): MarkGroup[] {
  const { timeWidth, futureWidth } = geometry(width);
  const groups: MarkGroup[] = [];
  for (const mark of [...marks].sort((a, b) => a.at - b.at || a.id.localeCompare(b.id))) {
    const x = LABEL_WIDTH + (mark.at / 1000 - view.start) / view.duration * timeWidth;
    if (x < LABEL_WIDTH || x > width - futureWidth) continue;
    const previous = groups.at(-1);
    if (previous && x - previous.x < 24) previous.marks.push(mark);
    else groups.push({ id: mark.id, x, marks: [mark] });
  }
  return groups;
}
