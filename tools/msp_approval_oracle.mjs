import { readFile, writeFile } from 'node:fs/promises';
import { Connection, Session } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
const queue = [], waiters = [], sent = [], receipts = [], io = [], errors = [];
let ended = false, nextId = 0, nextCommand = 0;
const push = frame => {
  const value = { done: false, value: JSON.stringify(frame) + '\n' };
  if (waiters.length) waiters.shift()(value); else queue.push(value);
};
const transport = {
  incoming: { [Symbol.asyncIterator]() { return this; }, next() {
    if (queue.length) return Promise.resolve(queue.shift());
    if (ended) return Promise.resolve({ done: true });
    return new Promise(resolve => waiters.push(resolve));
  } },
  async write(line) {
    const frame = JSON.parse(line);
    if (!frame.method) { receipts.push(frame); return; }
    if (frame.method !== 'approval/decide') throw new Error(`Unexpected method ${frame.method}`);
    sent.push(frame);
    const terminal = sent.length >= fixture.terminalAt;
    if (sent.length === 1 && fixture.updateBeforeAck) {
      push({ jsonrpc: '2.0', method: 'approval/updated', params: fixture.update });
      await new Promise(resolve => setTimeout(resolve, 50));
    }
    push({ jsonrpc: '2.0', id: frame.id, result: { commandId: frame.params.commandId,
      approvalId: fixture.request.approvalId, status: 'accepted', terminal } });
    if (sent.length === 1 && !fixture.updateBeforeAck)
      push({ jsonrpc: '2.0', method: 'approval/updated', params: fixture.update });
    if (terminal && fixture.duplicateRequest)
      push({ jsonrpc: '2.0', method: 'approval/requested', params: fixture.request });
  },
  close() { ended = true; for (const resolve of waiters.splice(0)) resolve({ done: true }); },
};
const connection = new Connection(transport, { mintRequestId: () => ++nextId,
  mintCommandId: () => `01900000-0000-7000-8000-${String(++nextCommand).padStart(12, '0')}` });
const session = new Session({ sessionId: fixture.session, durability: { kind: 'durable' }, connection });
session.onApproval(request => ({ choiceId: (request.availableChoices.find(choice => choice.decision === 'approved')
  ?? request.availableChoices.find(choice => choice.decision === 'denied')
  ?? request.availableChoices.find(choice => choice.decision === 'abort')).choiceId }));
session.onApprovalError(error => errors.push({ kind: error.kind }));
connection.onNotification(frame => io.push(session.apply(frame).io));
connection.onServerRequest(async frame => { io.push(session.apply(frame).io); return {}; });
connection.onProtocolError(error => errors.push({ kind: 'protocolError', message: String(error) }));
push(fixture.serverRequest
  ? { jsonrpc: '2.0', id: 1001, method: 'approval/request', params: fixture.request }
  : { jsonrpc: '2.0', method: 'approval/requested', params: fixture.request });
if (fixture.duplicateRequest) push({ jsonrpc: '2.0', method: 'approval/requested', params: fixture.request });
const deadline = Date.now() + 5000;
while (sent.length < fixture.terminalAt && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 2));
await Promise.all(io);
await new Promise(resolve => setTimeout(resolve, 200));
await connection.close();
await writeFile(process.argv[3], JSON.stringify({ sent, receipts, errors, complete: sent.length === fixture.terminalAt }) + '\n');
console.log(JSON.stringify({ decisions: sent.length, errors }));
