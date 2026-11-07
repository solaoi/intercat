import test from 'node:test';
import assert from 'node:assert/strict';
import { draftStore } from './drafts.mjs';

test('draft operations use the daemon and a close waits for its acknowledgement', async () => {
  const calls = [];
  let done;
  const gate = new Promise(resolve => { done = resolve; });
  const store = draftStore(async () => ({ request: async (method, params) => {
    calls.push({ method, params });
    if (method === 'draft.save') { await gate; return { status: 'saved' }; }
    return { revision: 0 };
  } }));
  const pending = store.write({ session: 'one' }, 'window');
  let drained = false;
  const flush = store.flush().then(() => { drained = true; });
  await new Promise(resolve => setTimeout(resolve, 5));
  assert.equal(drained, false);
  done(); await pending; await flush;
  await store.read('one');
  assert.deepEqual(calls.map(call => call.method), ['draft.save', 'draft.read']);
});
test('one window cannot discard another window’s failed save', async () => {
  let fail = true;
  const store = draftStore(async () => ({ request: async method => {
    if (method === 'draft.save') { if (fail) throw new Error('SQLITE_FULL'); return { status: 'saved' }; }
    return { revision: 1 };
  } }));
  await assert.rejects(store.write({ session: 's' }, 'a'));
  fail = false; await store.write({ session: 's' }, 'b');
  await assert.rejects(store.flush(), /SQLITE_FULL/);
  assert.equal((await store.discard('s', 'a')).revision, 1);
  await store.flush();
});
test('conflicts stay pending until that window saves or explicitly discards', async () => {
  let conflict = true;
  const store = draftStore(async () => ({ request: async method => method === 'draft.save' ? { status: conflict ? 'conflict' : 'saved' } : { revision: 2 } }));
  assert.equal((await store.write({ session: 's' }, 'a')).status, 'conflict');
  await assert.rejects(store.flush(), /another window/);
  conflict = false; await store.write({ session: 's' }, 'a');
  await store.flush();
});
