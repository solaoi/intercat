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
