// Compare the real worker's persisted history to the pinned official SDK fold.
import { readFile, writeFile } from 'node:fs/promises';
import { isDeepStrictEqual } from 'node:util';
import { Session } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

const directory = process.argv[2];
const result = JSON.parse(await readFile(`${directory}/result.json`, 'utf8'));
const frames = JSON.parse(await readFile(`${directory}/applied-frames.json`, 'utf8'));
const expected = JSON.parse(await readFile(`${directory}/reopened-view.json`, 'utf8'));
const session = new Session({
  sessionId: result.worker.sessionId,
  durability: { kind: 'durable' },
});
for (const frame of frames) {
  await session.apply(frame).io;
}
const fold = session.fold;
const actual = {
  items: fold.items.list(),
  turns: fold.turns(),
  activeTurnId: fold.activeTurnId ?? null,
  sessionState: Object.fromEntries(fold.sessionState.families().map(key => [key, fold.sessionState.get(key)])),
  pendingApprovals: fold.pendingApprovals(),
  resolvedApprovals: fold.resolvedApprovals(),
  pendingUserInputs: fold.pendingUserInputs(),
  settledUserInputs: fold.settledUserInputs(),
};
await writeFile(`${directory}/sdk-view.json`, JSON.stringify(actual) + '\n');
const differentFields = Object.keys(expected).filter(key => !isDeepStrictEqual(actual[key], expected[key]));
const comparison = { frames: frames.length, items: actual.items.length, turns: actual.turns.length, differentFields };
await writeFile(`${directory}/sdk-comparison.json`, JSON.stringify(comparison, null, 2) + '\n');
console.log(JSON.stringify(comparison));
if (differentFields.length) process.exitCode = 1;
