import { expect, test } from '@playwright/test';
import { mergeStatus } from '../src/status';
import type { DaemonStatus } from '../src/bridge';

function status(): DaemonStatus {
  return { connected: true, daemonPid: 17, sequence: 100, tracks: [{ id: 'a', label: 'A', title: 'Work', status: 'working' }], timeline: { clock: { epochMs: 100, monoMs: 100, bootId: 'boot', timelineMs: 100 }, summaries: [] } };
}
test('clock observations retain unchanged state without hiding transient changes', () => {
  const before = status(), after = structuredClone(before);
  after.timeline!.clock.timelineMs += 1000;
  expect(mergeStatus(before, after)).toBe(before);
  after.editor = { phase: 'ready', process: true, accepting: true, modelLoaded: true, generation: 1, restart: 0, canRetry: false };
  const changed = mergeStatus(before, after);
  expect(changed.editor).toEqual(after.editor);
  expect(changed.tracks).toBe(before.tracks);
  after.tracks![0].status = 'done';
  expect(mergeStatus(before, after).tracks![0].status).toBe('done');
});
test('a stale response is ignored but a restarted daemon can have a lower sequence', () => {
  const before = status(), after = structuredClone(before);
  after.sequence = 1;
  expect(mergeStatus(before, after)).toBe(before);
  after.daemonPid = 18;
  expect(mergeStatus(before, after).daemonPid).toBe(18);
});
