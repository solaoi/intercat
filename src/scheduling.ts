import type { DaemonStatus } from './bridge';

export function queueLabel(position: number) {
  const suffix = position % 100 >= 11 && position % 100 <= 13 ? 'th'
    : ({ 1: 'st', 2: 'nd', 3: 'rd' }[position % 10] ?? 'th');
  return `queued · ${position}${suffix}`;
}

export function queuedTracks(status: DaemonStatus) {
  const inputs = new Map(status.utterances?.map(word => [word.id, word]));
  const queued = new Map<string, number>();
  for (const entry of status.scheduling?.waiting ?? []) {
    const track = inputs.get(entry.id)?.decision?.trackId;
    if (track && !queued.has(track)) queued.set(track, entry.position);
  }
  return queued;
}

export function waitingForMemory(status: Pick<DaemonStatus, 'hostMemory'>, owner: string | undefined) {
  return !!owner && !!status.hostMemory?.waiting.some(entry => entry.owner === owner);
}
