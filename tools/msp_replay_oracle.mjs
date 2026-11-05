// Run the pinned TS facade over recorded sessions, without a live host or API.
import { readFile, writeFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { Session } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

function digest(value) {
  function encode(value) {
    if (value === null) return 'z';
    if (typeof value === 'boolean') return value ? 't' : 'f';
    if (typeof value === 'number') {
      const bytes = Buffer.alloc(8);
      bytes.writeDoubleBE(value);
      return 'n' + bytes.toString('hex');
    }
    if (typeof value === 'string') return 's' + JSON.stringify(value);
    if (Array.isArray(value)) return '[' + value.map(item => encode(item) + ',').join('') + ']';
    return '{' + Object.keys(value).sort().map(key => JSON.stringify(key) + ':' + encode(value[key]) + ',').join('') + '}';
  }
  return createHash('sha256').update(encode(value)).digest('hex');
}
function projection(session) {
  const fold = session.fold;
  return {
    items: fold.items.list(),
    turns: fold.turns(),
    activeTurnId: fold.activeTurnId ?? null,
    sessionState: Object.fromEntries(fold.sessionState.families().map(key => [key, fold.sessionState.get(key)])),
    pendingApprovals: fold.pendingApprovals(),
    resolvedApprovals: fold.resolvedApprovals(),
    pendingUserInputs: fold.pendingUserInputs(),
    settledUserInputs: fold.settledUserInputs(),
  };
}
const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
const results = [];
for (const test of fixture.cases) {
  const session = new Session({ sessionId: test.sessionId, durability: { kind: 'durable' } });
  const steps = [];
  for (const frame of test.frames) {
    const outcome = session.apply(frame);
    await outcome.io;
    steps.push(digest(projection(session)));
  }
  results.push({ id: test.id, steps, final: projection(session) });
}
await writeFile(process.argv[3], JSON.stringify({ cases: results }) + '\n');
console.log(JSON.stringify({ cases: results.length, steps: results.reduce((n, test) => n + test.steps.length, 0) }));
