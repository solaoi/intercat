import test from 'node:test';
import assert from 'node:assert/strict';
import { synchronizeFingerprints } from './secrets.mjs';
import { pickupFromFiles, cancelFileRecovery } from './pickup.mjs';

test('file recovery waits for external receipt confirmation and retries the same recovery', async () => {
  const proof = { sessions: [], projects: [] };
  const before = { revision: 1, digest: '1'.repeat(64) }, after = { revision: 2, digest: '2'.repeat(64) };
  let receipt = { catalog: before, proof }, seal = before, reject = true;
  const calls = [];
  const client = { async request(method, request) {
    calls.push({ method, request });
    if (method === 'secret.catalog') return { current: before, sealed: before, proof };
    if (method === 'worker.pickup.prepare') return { catalog: { current: after, sealed: seal, proof } };
    if (method === 'secret.confirm') { if (reject && request.revision === after.revision) { reject = false; throw new Error('confirmation lost'); } seal = request; return {}; }
    if (method === 'worker.pickup.activate') return { phase: 'ready' };
    throw new Error(method);
  } };
  const store = { run: fn => fn(), async read() { return receipt; }, async publish(expected, next) { assert.deepEqual(expected, receipt); receipt = next; calls.push({ method: 'receipt' }); } };
  await synchronizeFingerprints(client, store); calls.length = 0;
  const request = { opId: 'operation', trackId: 'track', sessionId: 'old-session' };
  await assert.rejects(pickupFromFiles(client, request), /confirmation lost/);
  assert.deepEqual(calls.map(c => c.method), ['worker.pickup.prepare', 'receipt', 'secret.confirm']);
  calls.length = 0;
  assert.deepEqual(await pickupFromFiles(client, request), { phase: 'ready' });
  assert.deepEqual(calls.map(c => c.method), ['worker.pickup.prepare', 'receipt', 'secret.confirm', 'worker.pickup.activate']);
  assert.deepEqual(calls[0].request, request);
});

test('a failed stop cannot publish a receipt or start a new worker', async () => {
  const seal = { revision: 0, digest: '0'.repeat(64) }, proof = { sessions: [], projects: [] }, calls = [];
  const client = { async request(method) { calls.push(method); if (method === 'secret.catalog') return { current: seal, sealed: null, proof }; if (method === 'secret.confirm') return {}; throw new Error('owned processes remain'); } };
  const store = { run: fn => fn(), async read() { return null; }, async publish() { calls.push('receipt'); } };
  await synchronizeFingerprints(client, store); calls.length = 0;
  await assert.rejects(pickupFromFiles(client, {}), /owned processes remain/);
  assert.deepEqual(calls, ['worker.pickup.prepare']);
});

test('cancellation bypasses a busy fingerprint lock then confirms the retained catalog', async () => {
  const seal = { revision: 0, digest: '0'.repeat(64) }, proof = { sessions: [], projects: [] }, calls = [];
  let receipt = null, tail = Promise.resolve(), started, failPrepare;
  const preparing = new Promise(resolve => { started = resolve; });
  const client = { async request(method, request) {
    calls.push({ method, request });
    if (method === 'secret.catalog') return { current: seal, sealed: null, proof };
    if (method === 'secret.confirm') return {};
    if (method === 'worker.pickup.prepare') { started(); return new Promise((_resolve, reject) => { failPrepare = reject; }); }
    if (method === 'worker.pickup.cancel') { failPrepare(new Error('recovery cancelled')); return { recovery: { phase: 'cancelled' }, catalog: { current: seal, sealed: seal, proof } }; }
    throw new Error(method);
  } };
  const store = {
    run(fn) { const next = tail.catch(() => {}).then(fn); tail = next; return next; },
    async read() { return receipt; }, async publish(expected, next) { assert.deepEqual(expected, receipt); receipt = next; calls.push({ method: 'receipt' }); },
  };
  await synchronizeFingerprints(client, store); calls.length = 0;
  const running = pickupFromFiles(client, { opId: 'recovery', trackId: 'track', sessionId: 'session' });
  const failed = assert.rejects(running, /cancelled/);
  await preparing;
  const request = { opId: 'cancel', recoveryId: 'recovery', trackId: 'track', sessionId: 'session' };
  assert.deepEqual(await cancelFileRecovery(client, request), { phase: 'cancelled' });
  await failed;
  assert.deepEqual(calls.map(c => c.method), ['worker.pickup.prepare', 'worker.pickup.cancel', 'receipt', 'secret.confirm']);
  assert.deepEqual(calls[1].request, request);
});

test('cancellation retries receipt confirmation without activating a new session', async () => {
  const before = { revision: 1, digest: '1'.repeat(64) }, after = { revision: 2, digest: '2'.repeat(64) }, proof = { sessions: [], projects: [] };
  let receipt = { catalog: before, proof }, reject = true;
  const requests = [];
  const client = { async request(method, request) {
    if (method === 'secret.catalog') return { current: before, sealed: before, proof };
    if (method === 'worker.pickup.cancel') { requests.push(request); return { recovery: { phase: 'cancelled' }, catalog: { current: after, sealed: before, proof } }; }
    if (method === 'secret.confirm') { if (request === after && reject) { reject = false; throw new Error('confirmation lost'); } return {}; }
    throw new Error(`Unexpected activation: ${method}`);
  } };
  const store = { run: fn => fn(), async read() { return receipt; }, async publish(_expected, next) { receipt = next; } };
  await synchronizeFingerprints(client, store);
  const request = { opId: 'cancel', recoveryId: 'recovery', trackId: 'track', sessionId: 'session' };
  await assert.rejects(cancelFileRecovery(client, request), /confirmation lost/);
  assert.deepEqual(await cancelFileRecovery(client, request), { phase: 'cancelled' });
  assert.deepEqual(requests, [request, request]);
  assert.deepEqual(receipt.catalog, after);
});
