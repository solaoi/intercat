import type { DaemonStatus } from '../bridge';
import type { Track } from '../timeline/model';
import { blockedDependencies, dependencyIds } from './dependencies';

export function addPlans(tracks: Track[], status: DaemonStatus): Track[] {
  const output = tracks.map(track => ({ ...track, planned: [...track.planned] }));
  const colors = [0x819af5, 0x70b8ab, 0xb497cf, 0xd3ac72];
  const plans = (status.plans ?? []).filter(plan => plan.generation.state === 'ready');
  const labels = new Map(tracks.map(track => [track.id, track.label]));
  for (const plan of plans) for (const item of plan.items) {
    if (item.trackRef.startsWith('new:')) labels.set(`${plan.id}/${item.trackRef}`, `New ${item.trackRef.slice(4)}`);
  }
  const name = (trackId: string) => labels.get(trackId) ?? '?';
  for (const plan of plans) {
    const key = (reference: string) => reference.startsWith('new:') ? `${plan.id}/${reference}` : reference;
    for (const item of plan.items) {
      if (!['planned', 'editing'].includes(item.state)) continue;
      const id = key(item.trackRef);
      if (!output.some(track => track.id === id)) output.push({ id, planId: plan.id, label: name(id), title: item.title, folder: status.project?.folders.find(folder => folder.id === item.folderId)?.name ?? 'Planned track', status: 'waiting', color: colors[output.length % colors.length], clips: [], planned: [], repairs: [] });
    }
  }
  for (const plan of plans) {
    const key = (reference: string) => reference.startsWith('new:') ? `${plan.id}/${reference}` : reference;
    for (const item of [...plan.items].sort((a, b) => a.order - b.order)) {
      if (!['planned', 'editing'].includes(item.state)) continue;
      const track = output.find(track => track.id === key(item.trackRef));
      if (!track) continue;
      const parents = dependencyIds(plan, item, status.plans ?? []).map(id => status.planDependencies?.[id]);
      // Lane order is already visible in the tile positions. Connect only
      // explicit dependencies while retaining every predecessor for waiting.
      const linkedParents = item.after.map(after => status.planDependencies?.[after.id]);
      const blocked = blockedDependencies(plan, item, status.plans ?? [], status.planDependencies ?? {});
      const cut = blocked.find(parent => parent.state === 'cut');
      const failed = blocked.find(parent => parent.state === 'failed');
      const waiting = parents.filter(parent => parent && parent.state !== 'succeeded');
      const after = item.state === 'editing' ? 'editing'
        : blocked.some(parent => parent.unavailable) ? 'held · dependency unavailable'
        : cut ? `held · ${name(cut.trackId)}’s item was cut`
        : failed ? `held · ${name(failed.trackId)}’s ${failed.unclear ? 'result unclear' : 'item didn’t finish'}`
        : parents.some(parent => !parent) ? 'held · dependency unavailable'
        : plan.state === 'held' && !item.startRequested ? 'held'
        : waiting.length ? `after ${[...new Set(waiting.map(parent => name(parent!.trackId)))].join(' + ')}`
        : plan.state === 'running' || item.startRequested ? 'queued' : 'waiting for your go';
      track.planned.push({ id: item.planItemId, planId: plan.id, label: item.title, after, blocked: blocked.length > 0,
        dependencies: linkedParents.filter(parent => parent && !parent.unavailable && parent.state !== 'cut').map(parent => ({ id: parent!.id, trackId: parent!.trackId, clipId: parent!.clipId, at: parent!.atMs === null ? null : parent!.atMs / 1000 })) });
    }
  }
  return output;
}
