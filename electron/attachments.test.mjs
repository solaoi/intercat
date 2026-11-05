import test from 'node:test';
import assert from 'node:assert/strict';
import { referenceImporter } from './attachments.mjs';

test('a lost import acknowledgement retries the same file and operation', async () => {
  let choices = 0;
  const calls = [];
  const run = referenceImporter({
    choose: async () => { choices++; return '/dummy/brief.txt'; },
    operationId: () => 'operation-one',
    client: async () => ({ request: async (method, request) => {
      calls.push({ method, request });
      if (calls.length === 1) throw new Error('acknowledgement lost');
      return { id: request.opId, name: 'brief.txt', byteLen: 4, sha256: 'a'.repeat(64) };
    } }),
  });
  await assert.rejects(run(), /acknowledgement lost/);
  assert.equal((await run()).name, 'brief.txt');
  assert.equal(choices, 1);
  assert.deepEqual(calls[0], calls[1]);
});

test('choosing another file abandons only the pending import request', async () => {
  let next = 0;
  const calls = [];
  const run = referenceImporter({
    choose: async () => `/dummy/file-${++next}.txt`, operationId: () => `id-${next}`,
    client: async () => ({ request: async (_method, request) => {
      calls.push(request);
      if (next === 1) throw new Error('cannot read');
      return { id: request.opId, name: 'second.txt', byteLen: 1, sha256: 'b'.repeat(64) };
    } }),
  });
  await assert.rejects(run());
  await run(true);
  assert.deepEqual(calls, [{ opId: 'id-1', path: '/dummy/file-1.txt' }, { opId: 'id-2', path: '/dummy/file-2.txt' }]);
});

test('cancelling the native chooser never starts an import', async () => {
  const run = referenceImporter({ choose: async () => null, operationId: () => assert.fail(), client: async () => assert.fail() });
  assert.equal(await run(), null);
});
