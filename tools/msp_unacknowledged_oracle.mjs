// Exercise the official Session's actual submit-memory and reconnect driver.
import { readFile, writeFile } from 'node:fs/promises';
import { Session, MspError } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
const cases = [];
for (const test of fixture.cases) {
  const operation = test.operations[0], sent = [];
  let lost = true;
  const connection = {
    mintCommandId: () => operation.commandId,
    async command(method, params) {
      sent.push({ method, params: structuredClone(params) });
      if (lost) { lost = false; throw new Error('The initial transport lost its acknowledgement'); }
      if (test.replayDisconnect) throw new Error('The replay transport closed');
      if (test.replayError) throw new MspError(test.replayError);
      return test.replayAck;
    },
  };
  const session = new Session({ sessionId: test.sessionId, durability: { kind: 'durable' }, connection });
  try { await session.sendUserTurn({ input: operation.input, composerInput: operation.input }); }
  catch { /* A lost transport retains the authored command and its exact params. */ }
  const before = structuredClone(session.pending.list());
  const state = test.snapshot;
  const join = state => ({
    activeTurn: state.activeTurn, queuedTurns: state.queuedTurns,
    userMessageCommandIds: state.items.filter(item => item.kind === 'userMessage' && item.commandId)
      .map(item => ({ commandId: item.commandId, itemId: item.itemId })),
    lastItemId: state.items.at(-1)?.itemId ?? null,
  });
  const retired = test.allowReplay ? [...await session.resolveSnapshotJoin(join(state))] : [];
  if (test.afterSnapshot?.activeTurn) retired.push(...await session.resolveSnapshotJoin(join(test.afterSnapshot)));
  const logicalState = retired.some(row => row.kind === 'materialized') ? 'running'
    : retired.length ? 'held' : 'pending';
  cases.push({ id: test.id, sent, before, retired, pending: session.pending.list(), logicalState });
}
await writeFile(process.argv[3], JSON.stringify({ cases }, null, 2) + '\n');
console.log(JSON.stringify({ cases: cases.length, replayed: cases.reduce((n, c) => n + c.sent.length - 1, 0) }));
