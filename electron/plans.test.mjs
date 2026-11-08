import test from 'node:test';
import assert from 'node:assert/strict';
import { commandPlan, observePlans, planWindowFocused } from './plans.mjs';

const id = '019a5555-0000-7000-8000-000000000001';
const planId = '00000000-0000-5000-8000-000000000002';
const request = action => ({ opId: id, planId, revision: 3, action });

test('plan observations require the exact live foreground window and contents', () => {
  const window = { isDestroyed: () => false, isVisible: () => true, isFocused: () => true, isMinimized: () => false };
  const sender = { isDestroyed: () => false, isFocused: () => true };
  assert.equal(planWindowFocused(window, sender, 'owner', 'owner'), true);
  assert.equal(planWindowFocused(null, sender, 'owner', 'owner'), false);
  for (const [method, value] of [['isDestroyed', true], ['isVisible', false], ['isFocused', false], ['isMinimized', true]]) {
    assert.equal(planWindowFocused({ ...window, [method]: () => value }, sender, 'owner', 'owner'), false);
  }
  for (const [method, value] of [['isDestroyed', true], ['isFocused', false]]) {
    assert.equal(planWindowFocused(window, { ...sender, [method]: () => value }, 'owner', 'owner'), false);
  }
  assert.equal(planWindowFocused(window, sender, 'new-owner', 'old-owner'), false);
});

test('the production plan entry forwards every action, including moves and one-job fallback', async () => {
  const calls = [];
  const connect = async () => ({ request: async (...args) => { calls.push(args); return 'saved'; } });
  const actions = [
    ...['review', 'start', 'hold', 'retry', 'run-as-one'].map(kind => ({ kind })),
    ...['cut', 'restore', 'retry-item'].map(kind => ({ kind, itemId: planId })),
    { kind: 'move', itemId: planId, direction: 'earlier' },
    { kind: 'move', itemId: planId, direction: 'later' },
    { kind: 'move-to', itemId: planId, trackId: id },
    { kind: 'unlock-and-move', itemId: planId, trackId: id },
    { kind: 'start-without', itemId: planId, dependencyId: id },
    { kind: 'retry-dependency', itemId: planId, dependencyId: id, clipId: planId },
    { kind: 'keep-assumption', assumptionId: planId },
    { kind: 'resolve-lock', unlock: true }, { kind: 'resolve-lock', unlock: false },
  ];
  for (const action of actions) assert.equal(await commandPlan(connect, request(action)), 'saved');
  assert.deepEqual(calls, actions.map(action => ['plan.command', request(action)]));
});

test('plan visibility also requires native focus after connection completes', async () => {
  let focus = true;
  const calls = [];
  const result = { countdowns: [], started: [] };
  const connect = async () => { focus = false; return { request: async (...args) => { calls.push(args); return result; } }; };
  const request = { idle: true, plans: [{ planId, revision: 3 }] };
  assert.equal(await observePlans(connect, request, { owner: id, sequence: 1, focused: () => focus }), result);
  assert.deepEqual(calls[0], ['plan.observe', { owner: id, sequence: 1, idle: false, plans: request.plans }]);
  let connected = false;
  await assert.rejects(observePlans(async () => { connected = true; }, { ...request, elapsed: 5000 }, { owner: id, sequence: 2, focused: () => true }), /Invalid plan observation/);
  assert.equal(connected, false);
});

test('invalid plan operations fail before connecting to the daemon', async () => {
  let connected = 0;
  const connect = async () => { connected++; throw new Error('must not connect'); };
  for (const invalid of [
    null, request({ kind: 'dependencies', itemId: planId, after: [] }),
    request({ kind: 'move', itemId: planId, direction: 'outside' }),
    request({ kind: 'move-to', itemId: planId, trackId: 'new:1' }),
    request({ kind: 'unlock-and-move', itemId: planId, trackId: 'new:1' }),
    request({ kind: 'run-as-one', executeAllowed: true }),
    request({ kind: 'retry-dependency', itemId: planId, dependencyId: id }),
    request({ kind: 'resolve-lock', unlock: 'yes' }),
    request({ kind: 'resolve-lock', unlock: true, trackId: id }),
    { ...request({ kind: 'hold' }), opId: planId },
    { ...request({ kind: 'hold' }), opId: [id] },
    { ...request({ kind: 'hold' }), revision: 0 },
  ]) await assert.rejects(commandPlan(connect, invalid), /Invalid plan action/);
  assert.equal(connected, 0);
});

test('plan replies keep their exact version and reject malformed or oversized content', async () => {
  const { replyPlan } = await import('./plans.mjs');
  const calls=[];
  const connect=async()=>({request:async(...args)=>{calls.push(args);return {state:'landed'};}});
  const reply={opId:id,planId,questionId:planId,revision:3,question:'Dummy plan question',text:'Use the dummy checklist.'};
  assert.deepEqual(await replyPlan(connect,reply),{state:'landed'});
  assert.deepEqual(calls,[['plan.reply',reply]]);
  for(const invalid of [{...reply,revision:0},{...reply,text:' '},{...reply,text:'あ'.repeat(11000)},{...reply,question:'あ'.repeat(2800)},{...reply,extra:true}]) {
    await assert.rejects(replyPlan(connect,invalid),/Invalid plan reply/);
  }
  assert.equal(calls.length,1);
});
