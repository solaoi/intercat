import type { BranchOrigin, BranchPoint } from '../bridge';
import { formatTime, geometry, LABEL_WIDTH, type TimelineView } from './model';

export interface BranchPreview { trackId: string; points: BranchPoint[]; selectedTurnId: string | null }
export interface BranchPointGroup { x: number; points: BranchPoint[]; selected: boolean }

export function groupBranchPoints(preview: BranchPreview, view: TimelineView, width: number): BranchPointGroup[] {
  const { timeWidth } = geometry(width);
  const groups = new Map<number, BranchPointGroup>();
  for (const point of preview.points) {
    if (!point.available || point.timelineAt === null) continue;
    const time = point.timelineAt / 1000;
    if (time < view.start || time > view.start + view.duration) continue;
    const x = LABEL_WIDTH + (time - view.start) / view.duration * timeWidth;
    const key = Math.round((x - LABEL_WIDTH) / 10);
    const group = groups.get(key) ?? { x, points: [], selected: false };
    group.points.push(point);
    group.selected ||= point.turnId === preview.selectedTurnId;
    if (point.turnId === preview.selectedTurnId) group.x = x;
    groups.set(key, group);
  }
  return [...groups.values()].sort((a, b) => a.x - b.x);
}

export function branchContext(origin: BranchOrigin, parent: string): string {
  return origin.turnNumber === 0 ? `context before ${parent}’s first step` : origin.endedAt === null ? `context from ${parent}’s turn ${origin.turnNumber}` : `context from ${formatTime(origin.endedAt / 1000)}`;
}
