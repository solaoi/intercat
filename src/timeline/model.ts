import type { ClipWarning, TrackCompletion } from '../bridge';

export type TrackStatus = 'working' | 'waiting' | 'done' | 'stopped';
export interface Clip {
  id: string;
  label: string;
  start: number;
  end: number;
  kind: 'read' | 'think' | 'edit' | 'shell';
  ordinal?: number;
  effectCount?: number;
  assumptionCount?: number;
  assumptionTimes?: number[];
  warnings?: ClipWarning[];
  running?: boolean;
  generationEnd?: number | null;
  approximate?: boolean;
  toolTicks?: { itemId: string; tool: string; status: string }[];
}
export interface PlannedTurn { id: string; label: string; after: string }
export interface RepairLane { id: string; status: TrackStatus; canStop: boolean; clips: Clip[] }
export interface Track {
  id: string;
  label: string;
  title: string;
  folder: string;
  status: TrackStatus;
  completion?: TrackCompletion | null;
  nextWaitsForAnswer?: boolean;
  color: number;
  clips: Clip[];
  planned: PlannedTurn[];
  repairs: RepairLane[];
}
export interface TimelineView {
  start: number;
  duration: number;
  now: number;
}

const SAMPLE_START = new Date(2026, 0, 1, 14, 0, 0).getTime() / 1000;
const sampleOffsets: Omit<Track, 'repairs'>[] = [
  { id: 'A', label: 'A', title: 'login retry', folder: 'sample-web', status: 'working', color: 0x819af5,
    clips: [
      { id: 'a1', label: 'read login.ts', start: 48, end: 96, kind: 'read' },
      { id: 'a2', label: 'thinking', start: 99, end: 153, kind: 'think' },
      { id: 'a3', label: 'patch retry', start: 156, end: 228, kind: 'edit' },
      { id: 'a4', label: 'run tests', start: 231, end: 288, kind: 'shell' },
      { id: 'a5', label: 'patch: use services/logger', start: 291, end: 348, kind: 'edit' },
      { id: 'a6', label: 'thinking', start: 351, end: 384, kind: 'think' },
    ], planned: [{ id: 'p1', label: 'changelog', after: 'after this turn' }, { id: 'p2', label: 'run tests', after: 'after changelog' }] },
  { id: 'B', label: 'B', title: 'rate limits', folder: 'sample-api', status: 'waiting', color: 0x70b8ab,
    clips: [
      { id: 'b1', label: 'read middleware', start: 156, end: 225, kind: 'read' },
      { id: 'b2', label: 'thinking', start: 228, end: 276, kind: 'think' },
      { id: 'b3', label: 'add limiter', start: 279, end: 351, kind: 'edit' },
      { id: 'b4', label: 'run tests', start: 354, end: 384, kind: 'shell' },
    ], planned: [{ id: 'p3', label: 'verify response headers', after: 'after this turn' }] },
  { id: 'C', label: 'C', title: 'edge cases', folder: 'sample-web', status: 'done', color: 0xb497cf,
    clips: [
      { id: 'c1', label: 'read tests', start: 60, end: 120, kind: 'read' },
      { id: 'c2', label: 'add regression tests', start: 123, end: 237, kind: 'edit' },
      { id: 'c3', label: 'tests passed', start: 240, end: 303, kind: 'shell' },
    ], planned: [] },
];

export const sampleTracks: Track[] = sampleOffsets.map(track => ({ ...track, repairs: [], clips: track.clips.map(clip => ({ ...clip, start: clip.start + SAMPLE_START, end: clip.end + SAMPLE_START })) }));
export const initialView: TimelineView = { start: SAMPLE_START, duration: 480, now: SAMPLE_START + 384 };
export const LABEL_WIDTH = 180;
export const TOP_HEIGHT = 112;
export const ROW_HEIGHT = 76;
export const REPAIR_ROW_HEIGHT = 26;

export function trackLayout(tracks: Track[]) {
  let top = TOP_HEIGHT;
  return tracks.map(track => {
    const row = { track, top, repairs: track.repairs.map((repair, index) => ({ repair, top: top + ROW_HEIGHT + index * REPAIR_ROW_HEIGHT })) };
    top += ROW_HEIGHT + track.repairs.length * REPAIR_ROW_HEIGHT;
    return row;
  });
}
export function timelineHeight(tracks: Track[]) {
  return TOP_HEIGHT + tracks.reduce((sum, track) => sum + ROW_HEIGHT + track.repairs.length * REPAIR_ROW_HEIGHT, 0);
}

export function cutPlannedTurn(tracks: Track[], id: string): Track[] {
  return tracks.map(track => {
    if (!track.planned.some(turn => turn.id === id)) return track;
    const remaining = track.planned.filter(turn => turn.id !== id);
    return { ...track, planned: remaining.map((turn, index) => ({ ...turn, after: index === 0 ? 'after this turn' : `after ${remaining[index - 1].label}` })) };
  });
}

export function geometry(width: number) {
  const laneWidth = Math.max(0, width - LABEL_WIDTH);
  const futureCount = laneWidth < 49 ? 0 : laneWidth >= 900 ? 2 : 1;
  const tileWidth = futureCount ? Math.min(152, (laneWidth - 17) / futureCount - 8) : 0;
  const futureWidth = futureCount ? futureCount * (tileWidth + 8) + 16 : 0;
  return { laneWidth, futureCount, tileWidth, futureWidth, timeWidth: Math.max(1, laneWidth - futureWidth) };
}

// The current page remains steady until the playhead crosses 92%. The next
// page puts it back at 80%; no timestamp is drawn beyond the playhead.
export function advanceView(view: TimelineView, seconds: number, interacting: boolean): TimelineView {
  const now = view.now + seconds;
  const ratio = (now - view.start) / view.duration;
  const start = !interacting && ratio >= 0.92 ? now - view.duration * 0.8 : view.start;
  return { ...view, now, start };
}

export function formatTime(seconds: number) {
  const date = new Date(Math.floor(seconds) * 1000);
  return [date.getHours(), date.getMinutes(), date.getSeconds()].map(value => String(value).padStart(2, '0')).join(':');
}
