import { expect, test } from '@playwright/test';
import { mergeStatus } from '../src/status';
import type { DaemonStatus } from '../src/bridge';
import type { Plan } from '../src/plans/types';

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
test('a plan revision from a command survives an older status response', () => {
  const held = { id: 'dummy-plan', rev: 3, state: 'held' } as Plan;
  const before = { ...status(), plans: [held] };
  const incoming = { ...status(), plans: [{ ...held, rev: 2, state: 'draft' as const }] };
  expect(mergeStatus(before, incoming).plans?.[0]).toBe(held);
  expect(mergeStatus(before, { ...incoming, daemonPid: 18 }).plans?.[0].rev).toBe(2);
  expect(mergeStatus(before, { ...incoming, plans: [] }).plans).toEqual([]);
});

test('paired command revisions survive older status while removal, another board, and daemon restarts remain authoritative',()=>{
  const job={request:{opId:'own-pair'},revision:4,phase:'cancelling'} as NonNullable<DaemonStatus['twoWays']>[number];
  const before={...status(),session:{id:'own-board',name:'Own board',slug:'own',createdAt:1},twoWays:[job]};
  const incoming={...before,sequence:101,twoWays:[{...job,revision:3,phase:'forking' as const}]};
  expect(mergeStatus(before,incoming).twoWays?.[0]).toBe(job);
  expect(mergeStatus(before,{...incoming,twoWays:[]}).twoWays).toEqual([]);
  expect(mergeStatus(before,{...incoming,daemonPid:18}).twoWays?.[0].revision).toBe(3);
  expect(mergeStatus(before,{...incoming,session:{...before.session,id:'other-board'}}).twoWays?.[0].revision).toBe(3);
});
