import test from 'node:test';
import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { RpcClient } from './rpc-client.mjs';

test('RPC rejection codes distinguish a recoverable registration refusal from connection loss', async () => {
  const socket = new EventEmitter();
  socket.setEncoding = () => {};
  socket.write = frame => {
    const { id } = JSON.parse(frame);
    queueMicrotask(() => socket.emit('data', JSON.stringify({ id, error: { code: -32002, message: 'Project fingerprint capacity reached.' } }) + '\n'));
  };
  const client = new RpcClient(socket);
  await assert.rejects(client.request('secret.project.check', {}), { code: -32002 });
  socket.write = () => { queueMicrotask(() => socket.emit('close')); };
  await assert.rejects(client.request('secret.project.check', {}), error => error.code === undefined && /connection closed/u.test(error.message));
});
test('pasted text inspection can use its full scanner deadline', async t => {
  const timeouts = [];
  const setTimer = globalThis.setTimeout;
  t.mock.method(globalThis, 'setTimeout', (callback, delay, ...args) => {
    timeouts.push(delay);
    return setTimer(callback, delay, ...args);
  });
  const socket = new EventEmitter();
  socket.setEncoding = () => {};
  socket.write = frame => {
    const { id } = JSON.parse(frame);
    queueMicrotask(() => socket.emit('data', JSON.stringify({ id, result: {} }) + '\n'));
  };
  const client = new RpcClient(socket);
  await client.request('attachment.paste');
  await client.request('attachment.paste.refresh');
  await client.request('attachment.paste.list');
  assert.deepEqual(timeouts, [45000, 45000, 15000]);
});
