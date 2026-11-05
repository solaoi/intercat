// Real SDK handshake against the same offline child used by the Rust tests.
import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { resolve } from 'node:path';
import { spawnMspConnection, EXPECTED_SCHEMA_FINGERPRINT } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

const directory = resolve(process.argv[2]);
const fixturePath = `${directory}/fixture.json`;
const fixture = JSON.parse(await readFile(fixturePath, 'utf8'));
await mkdir(`${directory}/ts`, { recursive: true });
const results = [];
for (const test of fixture.cases) {
  const transcript = `${directory}/ts/${test.id}.jsonl`;
  await writeFile(transcript, '');
  const handshake = spawnMspConnection({ command: process.execPath,
    args: [resolve('tools/fake_msp_handshake.mjs'), fixturePath, test.id, transcript],
    shutdownTimeoutMs: 3000 });
  let phase = 'initialize', outcome;
  try {
    const opened = await handshake.initialize(fixture.initializeParams);
    if (test.kind === 'handshake') {
      phase = 'repeat';
      let repeated;
      try { await handshake.initialize(fixture.initializeParams); repeated = { ok: true }; }
      catch (error) { repeated = { ok: false, error: error.message }; }
      outcome = { ok: true, result: opened.initializeResult, repeated };
    } else {
      // Intercat's explicit fixed-binary admission requirements are stricter
      // than the SDK's additive-version and absent-durability defaults.
      phase = 'fingerprint-policy';
      if (opened.initializeResult.schema.fingerprint !== EXPECTED_SCHEMA_FINGERPRINT) throw new Error('Unverified fingerprint');
      phase = 'durability';
      if (opened.initializeResult.sessionDurability !== 'durable') throw new Error('Durable session required');
      phase = 'session/start';
      const session = await opened.connection.command('session/start', { workspaceRoot: fixture.workspace,
        approvalMode: 'promptUnmatched', providerId: 'meta', modelId: 'muse-spark-1.3-contributor', recoveryMode: 'on' });
      phase = 'profile';
      if (session.session?.permissionState?.profileId !== 'intercat-worker') throw new Error('Wrong profile');
      phase = 'session-id';
      if (typeof session.session?.sessionId !== 'string') throw new Error('Session ID absent');
      phase = 'skill/list';
      const skills = await opened.connection.request('skill/list', { sessionId: session.session.sessionId });
      phase = 'report-skill';
      if (!skills.skills?.some(skill => skill.selector === 'intercat-report')) throw new Error('Report skill absent');
      outcome = { ok: true, sessionId: session.session.sessionId };
    }
  } catch (error) {
    outcome = { ok: false, phase, error: error.message, errorKind: error.constructor.name };
  }
  const exit = await handshake.close();
  const sent = (await readFile(transcript, 'utf8')).trim().split('\n').filter(Boolean).map(JSON.parse);
  results.push({ id: test.id, outcome, sent, exit });
}
await writeFile(`${directory}/ts.json`, JSON.stringify({ cases: results }, null, 2) + '\n');
console.log(JSON.stringify({ cases: results.length, accepted: results.filter(c => c.outcome.ok).length }));
