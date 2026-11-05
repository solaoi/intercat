import { expect, test } from '@playwright/test';
import { advanceView, cutPlannedTurn, geometry, initialView, sampleTracks } from '../src/timeline/model';

test('future tiles stay fixed while the time area responds to window size', () => {
  const small = geometry(788);
  const wide = geometry(1150);
  expect(small.futureCount).toBe(1);
  expect(wide.futureCount).toBe(2);
  expect(small.tileWidth).toBe(wide.tileWidth);
  expect(small.timeWidth + small.futureWidth + 180).toBe(788);
});
test('page turns at 92% to place now at 80%, and waits during interaction', () => {
  expect(advanceView(initialView, 50, false).start).toBe(initialView.start);
  const turned = advanceView(initialView, 60, false);
  expect((turned.now - turned.start) / turned.duration).toBe(0.8);
  expect(advanceView(initialView, 100, true).start).toBe(initialView.start);
});
test('cutting a planned turn preserves current work and relabels the remaining queue', () => {
  const result = cutPlannedTurn(sampleTracks, 'p1');
  expect(result[0].clips).toBe(sampleTracks[0].clips);
  expect(result[0].status).toBe('working');
  expect(result[0].planned).toEqual([{ id: 'p2', label: 'run tests', after: 'after this turn' }]);
  expect(result[1]).toBe(sampleTracks[1]);
});

test('the selected row can stay mounted while the visible window moves elsewhere', async () => {
  const { lanes, visibleLanes, revealLane, totalHeight } = await import('../src/timeline/viewport');
  const tracks = Array.from({ length: 300 }, (_, index) => ({ ...sampleTracks[0], id: `track-${index}`, repairs: [] }));
  const rows = lanes(tracks);
  expect(totalHeight(rows)).toBe(300 * 76);
  expect(visibleLanes(rows, 100 * 76, 3 * 76, 0).map(row => row.index)).toEqual([100, 101, 102]);
  expect(revealLane(0, 3 * 76, rows[299])).toBe(297 * 76);
  expect(revealLane(297 * 76, 3 * 76, rows[0])).toBe(0);
  expect(lanes(tracks, true)[299].top).toBe(299 * 12);
});
