import { expect, test } from '@playwright/test';
import type { BranchPoint, BranchState, DaemonStatus } from '../src/bridge';
import { groupBranchPoints } from '../src/timeline/branchPointLayout';
import { initialView, geometry, LABEL_WIDTH } from '../src/timeline/model';
import { liveTimeline } from '../src/timeline/state';

test('nearby saved turns form one choice while the selected point stays at its exact time', () => {
  const point = (id: number, seconds: number): BranchPoint => ({ id, turnId: `turn-${id}`, checkpoint: String(id).repeat(40), endedAt: (initialView.start + seconds) * 1000, timelineAt: (initialView.start + seconds) * 1000, turnNumber: id, available: true, shellWasRunning: false });
  const selected = point(1, 60), nearby = point(2, 61), separate = point(3, 240);
  const unavailable = { ...point(4, 120), available: false };
  const unknown = { ...point(5, 180), timelineAt: null };
  const groups = groupBranchPoints({ trackId: 'A', points: [nearby, selected, unavailable, unknown, separate], selectedTurnId: selected.turnId }, initialView, 1000);
  expect(groups).toHaveLength(2);
  expect(groups[0].points.map(point => point.turnId)).toEqual(['turn-2', 'turn-1']);
  expect(groups[0].selected).toBe(true);
  expect(groups[0].x).toBeCloseTo(LABEL_WIDTH + 60 / initialView.duration * geometry(1000).timeWidth);
  expect(groups[1].selected).toBe(false);
});

test('a branch stays below its source with a recorded context and stable colors for other workers', () => {
  const tracks = ['A', 'B', 'A2', 'A3'].map(id => ({ id, label: id, title: `Generated ${id}`, status: 'done' }));
  const branch = (trackId: string, parentTrackId: string, endedAt: number | null): BranchState => ({
    request: { opId: `dummy-${trackId}`, parentTrackId, point: {kind:'turn', turnId:'dummy-turn'}, instructions: 'checkpoint' }, trackId, parentSessionId: 'dummy-parent', sessionId: 'dummy-child',
    workspace: { trackId, path: '/private/tmp/generated', reference: `refs/intercat/generated/${trackId}`, base: 'a'.repeat(40), deniedRoots: [], attachmentRoots: [], state: 'ready', error: null },
    point: {kind:'turn', checkpoint: { turnId: 'dummy-turn', sha: 'b'.repeat(40), reference: 'refs/intercat/generated/checkpoint', unmergedPaths: [], liveProcesses: false }},
    instructions: null, origin: { endedAt, timelineAt: endedAt, turnNumber: 3 }, phase: 'ready',
  });
  const status: DaemonStatus = { connected: true, tracks, branches: [branch('A2', 'A', new Date(2026, 0, 1, 14, 4, 27).getTime()), branch('A3', 'A2', null)] };
  const before = liveTimeline({ connected: true, tracks }, []);
  const after = liveTimeline(status, []);
  expect(after.map(track => track.id)).toEqual(['A', 'A2', 'A3', 'B']);
  expect(after.find(track => track.id === 'B')!.color).toBe(before.find(track => track.id === 'B')!.color);
  expect(after[1].branchContext).toBe('context from 14:04:27');
  expect(after[2].branchContext).toBe('context from A2’s turn 3');
});
