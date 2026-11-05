// Same fake host, with the pinned official TypeScript connection as oracle.
import { readFile } from 'node:fs/promises';
import { Connection, EXPECTED_SCHEMA_FINGERPRINT } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
if (fixture.calls[0].result.schema.fingerprint !== EXPECTED_SCHEMA_FINGERPRINT) throw new Error('Fixture schema fingerprint is not pinned');
const incoming = [], waiters = [];
let ended = false;
function push(value) {
  const result = { value: `${JSON.stringify(value)}\n`, done: false };
  if (waiters.length) waiters.shift()(result); else incoming.push(result);
}
const sent = [];
let call = 0, id = 0, request = 0;
const transport = {
  incoming: { [Symbol.asyncIterator]() { return this; }, next() {
    if (incoming.length) return Promise.resolve(incoming.shift());
    if (ended) return Promise.resolve({ done: true });
    return new Promise(resolve => waiters.push(resolve));
  } },
  async write(line) {
    const frame = JSON.parse(line); sent.push(frame);
    if ('id' in frame && 'method' in frame) push({ jsonrpc: '2.0', id: frame.id, result: fixture.calls[call++].result });
    else if ('id' in frame && ++request < fixture.requests.length) push(fixture.requests[request].frame);
  },
  close() { ended = true; for (const resolve of waiters.splice(0)) resolve({ done: true }); },
};
const connection = new Connection(transport, { mintRequestId: () => ++id });
const notifications = [];
const requests = [];
let protocolError;
connection.onProtocolError(error => { protocolError = error; });
connection.onNotification(frame => notifications.push(frame));
connection.onServerRequest(async frame => {
  requests.push(frame);
  if (frame.method === 'userInput/request') connection.notify('userInput/engaged', { sessionId: frame.params.sessionId, userInputId: frame.params.userInputId });
  return {};
});
const results = [];
for (const call of fixture.calls) results.push(await connection.request(call.method, call.params));
connection.notify(fixture.notify.method, fixture.notify.params);
await connection.flush();
for (const { frame } of fixture.notifications) push(frame);
push(fixture.requests[0].frame);
const deadline = Date.now() + 10000;
while (notifications.length < fixture.notifications.length || request < fixture.requests.length) {
  if (protocolError) throw protocolError;
  if (Date.now() > deadline) throw new Error('Fixture did not settle');
  await new Promise(resolve => setTimeout(resolve, 1));
}
await connection.close();
console.log(JSON.stringify({ sent, results, notifications, requests, schemaFingerprint: EXPECTED_SCHEMA_FINGERPRINT }));
