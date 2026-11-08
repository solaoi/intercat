import { geometry, LABEL_WIDTH, type PlannedTurn, type TimelineView } from './model';
import type { Lane } from './viewport';

export interface Point { x: number; y: number }
export interface DependencyLink { id: string; from: string; to: string; description: string; points: Point[] }
interface Anchor extends Point { grouped: boolean; label: string }

/** All lanes use their own first column, regardless of dependency depth. */
export function plannedAnchor(row: Lane, index: number, width: number): Anchor | null {
  const layout = geometry(width), item = row.track.planned[index];
  if (!layout.futureCount || !item) return null;
  const grouped = index >= layout.futureCount;
  return { x: grouped ? width - 20 : width - layout.futureWidth + 8 + index * (layout.tileWidth + 8) + layout.tileWidth / 2,
    y: row.top + (grouped ? 60 : 15), grouped, label: item.label };
}

export function dependencyLinks(rows: Lane[], width: number, view: TimelineView, selected: string | null): DependencyLink[] {
  if (!selected) return [];
  const layout = geometry(width), right = width - layout.futureWidth;
  const lanes = new Map(rows.filter(row => !row.repairId).map(row => [row.track.id, row]));
  const items = new Map<string, { row: Lane; item: PlannedTurn; anchor: Anchor }>();
  for (const row of lanes.values()) row.track.planned.forEach((item,index) => {
    const anchor = plannedAnchor(row,index,width);
    if (anchor) items.set(item.id,{row,item,anchor});
  });
  const links: DependencyLink[] = [];
  for (const { row, item, anchor: target } of items.values()) for (const dependency of item.dependencies ?? []) {
    if (item.id !== selected && dependency.id !== selected) continue;
    const parent = items.get(dependency.id);
    const sourceRow = parent?.row ?? lanes.get(dependency.trackId);
    if (!sourceRow) continue;
    const source = parent?.anchor ?? (dependency.at === null ? null : {
      x: Math.max(LABEL_WIDTH + 2, Math.min(right - 2, LABEL_WIDTH + (dependency.at - view.start) / view.duration * layout.timeWidth)),
      y: sourceRow.top + 53, grouped: false, label: sourceRow.track.label,
    });
    if (!source) continue;
    // The horizontal legs stay in row gutters, outside the tile text.
    const sourceGutter = sourceRow.top + (parent && !source.grouped ? 8 : 68);
    const targetGutter = row.top + (target.grouped ? 68 : 8);
    const channel = right + 3;
    links.push({ id:`${dependency.id}/${item.id}`, from:dependency.id, to:item.id,
      description:`${source.label} → ${item.label}`, points:[{x:source.x,y:source.y},{x:source.x,y:sourceGutter},{x:channel,y:sourceGutter},{x:channel,y:targetGutter},{x:target.x,y:targetGutter},{x:target.x,y:target.y}] });
  }
  return links;
}
