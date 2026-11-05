// SDK-backed rendering projection: only the caller's active turn, the newest
// eligible agent item, and a bounded UTF-8 prefix. Authoritative item bodies
// replace the preview; new deltas extend that body after its observed offset.
import { readFile, writeFile } from 'node:fs/promises';
import { Session } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';
import { isItemInProgress } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/facade/host-death.js';

const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
const cases = [];
function prefix(text) {
  let result = '', bytes = 0;
  for (const char of text) {
    const size = Buffer.byteLength(char);
    if (bytes + size > 65536) break;
    result += char;
    bytes += size;
  }
  return { text: result, truncated: result !== text };
}
for (const test of fixture.cases) {
  const session = new Session({ sessionId: test.sessionId, durability: { kind: 'durable' } });
  const bases = new Map();
  let active = test.initialTurn, latest;
  const steps = [];
  const finalized = [];
  for (const action of test.actions) {
    if (action.send) {
      if (active !== action.send.commandId) {
        active = action.send.commandId;
        bases.clear();
        latest = undefined;
      }
    } else if (action.frame.params?.sessionId === test.sessionId) {
      const frame = action.frame;
      const applied = session.apply(frame);
      await applied.io;
      const outcome = applied.fold;
      const params = frame.params;
      if (frame.method === 'turn/completed' && params.turnId === active) {
        active = null;
        bases.clear();
        latest = undefined;
      } else if (['view/gap', 'session/viewHealthChanged'].includes(frame.method)) {
        bases.clear();
        latest = undefined;
      } else if (outcome.kind === 'item' && outcome.outcome.kind !== 'ignoredStaleRevision') {
        const item = session.fold.items.get(params.item.itemId);
        if (item.kind === 'agentMessage' && item.turnId === active) {
          const accumulated = session.fold.items.accumulated(item.itemId) ?? '';
          const first = outcome.outcome.kind === 'inserted';
          bases.set(item.itemId, {
            text: item.text ?? '', offset: first && !item.text ? 0 : accumulated.length,
            terminal: !isItemInProgress(item),
          });
          latest = item.itemId;
        }
      } else if (frame.method === 'item/delta' && (params.field ?? 'text') === 'text') {
        const item = session.fold.items.get(params.itemId);
        if (item?.kind === 'agentMessage' && item.turnId === active && bases.has(item.itemId) && !bases.get(item.itemId).terminal) {
          latest = item.itemId;
        }
      }
    }
    const base = bases.get(latest);
    let view = null;
    if (base) {
      const item = session.fold.items.get(latest);
      const suffix = base.terminal ? '' : (session.fold.items.accumulated(latest) ?? '').slice(base.offset);
      const bounded = prefix(base.text + suffix);
      if (bounded.text) view = { sessionId: test.sessionId, itemId: latest, turnId: item.turnId, ...bounded };
    }
    steps.push(view);
    finalized.push(Boolean(base?.terminal));
  }
  cases.push({ id: test.id, steps, finalized });
}
await writeFile(process.argv[3], JSON.stringify({ cases }) + '\n');
console.log(JSON.stringify({ cases: cases.length, steps: cases.reduce((n, c) => n + c.steps.length, 0) }));
