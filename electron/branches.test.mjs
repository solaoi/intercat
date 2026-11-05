import test from 'node:test';
import assert from 'node:assert/strict';
import { synchronizeFingerprints } from './secrets.mjs';
import { branchFromCheckpoint } from './branches.mjs';
const proof = { sessions: [], projects: [] };
const wrap = catalog => ({ catalog, proof });

test('branch activation follows the external receipt and daemon confirmation', async () => {
  const before = { revision: 1, digest: '1'.repeat(64) }, after = { revision: 2, digest: '2'.repeat(64) };
  const calls = []; let receipt = wrap(before); let seal = before; let fail = true;
  const client = { async request(method) {
    calls.push(method);
    if (method === 'secret.catalog') return { current: before, sealed: before, proof };
    if (method === 'branch.prepare') return { catalog: { current: after, sealed: seal, proof } };
    if (method === 'branch.activate') return { phase: 'ready' };
    if (method === 'secret.confirm') { if (fail && receipt.catalog === after) { fail = false; throw new Error('lost confirmation'); } seal = receipt.catalog; return {}; }
    throw new Error(method);
  } };
  const store = { async read() { return receipt; }, async publish(expected, next) { assert.deepEqual(expected, receipt); receipt = next; calls.push('keychain.publish'); }, run(fn) { return fn(); } };
  await synchronizeFingerprints(client, store);
  calls.length = 0;
  await assert.rejects(branchFromCheckpoint(client, { opId: 'operation' }), /lost confirmation/);
  assert.deepEqual(calls, ['branch.prepare', 'keychain.publish', 'secret.confirm']);
  calls.length = 0;
  assert.deepEqual(await branchFromCheckpoint(client, { opId: 'operation' }), { phase: 'ready' });
  assert.deepEqual(calls, ['branch.prepare', 'keychain.publish', 'secret.confirm', 'branch.activate']);
});

test('an older database cannot activate a child behind a newer external receipt', async () => {
  const before = { revision: 1, digest: '1'.repeat(64) }, after = { revision: 2, digest: '2'.repeat(64) };
  let catalog = { current: after, sealed: after, proof }; const calls = [];
  const client = { async request(method) { calls.push(method); if (method === 'secret.catalog') return catalog; if (method === 'branch.prepare') return { catalog }; return {}; } };
  const store = { async read() { return wrap(after); }, async publish() {}, run(fn) { return fn(); } };
  await synchronizeFingerprints(client, store);
  calls.length = 0; catalog = { current: before, sealed: before, proof };
  await assert.rejects(branchFromCheckpoint(client, { opId: 'operation' }), { code: 'FINGERPRINT_RECEIPT_MISMATCH' });
  assert.deepEqual(calls, ['branch.prepare']);
});
