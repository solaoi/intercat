import { readFile, writeFile } from 'node:fs/promises';
import { SessionFold } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/fold/session-fold.js';

const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
const cases = [];
function families(state, sessionId, cursor) {
  const result = [];
  for (const [field, method, wrapper] of [
    ['effectiveModel','session/modelChanged'], ['reasoningEffort','session/reasoningEffortChanged'],
    ['goal','session/goalChanged','goal'], ['todoList','session/todoListChanged'],
    ['branch','session/branchChanged'], ['name','session/nameChanged','name'],
    ['tokenUsage','session/tokenUsage','cumulative'], ['contextUsage','session/contextUsage'],
    ['approvalMode','session/approvalModeChanged']]) {
    if (!(field in state) || state[field] === null && field !== 'goal') continue;
    result.push([method, {sessionId, viewCursor:cursor,
      ...(wrapper ? {[wrapper]:state[field]} : state[field])}]);
  }
  return result;
}
for (const c of fixture.cases) {
  const fold = new SessionFold(c.sessionId);
  for (const frame of c.before) fold.apply(frame);
  const snapshot = c.read.history.snapshot;
  fold.items.seed(snapshot.state.items);
  fold.sessionState.seed(families(snapshot.state,c.sessionId,snapshot.viewCursor),snapshot.viewCursor);
  for (const frame of [...c.after,...c.suffix]) fold.apply(frame);
  cases.push({id:c.id,items:fold.items.list(),sessionState:Object.fromEntries(
    fold.sessionState.families().map(k=>[k,fold.sessionState.get(k)]))});
}
await writeFile(process.argv[3], JSON.stringify({scope:'official item and session-state stores, not facade snapshot ingestion',cases})+'\n');
console.log(JSON.stringify({cases:cases.length}));
