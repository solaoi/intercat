// An offline MSP peer. Its only writes are the requested transcript and stdout.
import { readFileSync, appendFileSync } from 'node:fs';
import { createInterface } from 'node:readline';

const [fixturePath, caseId, transcript] = process.argv.slice(2);
const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
const test = fixture.cases.find(c => c.id === caseId);
if (!test) throw new Error('Unknown fixture case');
let initialized = false;
const lines = createInterface({ input: process.stdin });
process.stdin.on('error', () => {});
lines.on('error', () => {});
lines.on('close', () => process.exit(0));
function send(frame) { process.stdout.write(JSON.stringify(frame) + '\n'); }
lines.on('line', line => {
  const frame = JSON.parse(line);
  appendFileSync(transcript, JSON.stringify(frame) + '\n');
  if (frame.method === 'initialize') {
    if (test.behavior === 'exit') process.exit(test.exitCode);
    if (test.behavior === 'malformed') return process.stdout.write('{bad json\n', () => process.exit(0));
    if (test.behavior === 'initialize-error') return send({ jsonrpc: '2.0', id: frame.id,
      error: { code: -32602, message: 'fixture initialization refused', data: { kind: 'invalidParams' } } });
    if (test.behavior === 'close-stdin') {
      // Destroying the pipe before replying makes the following client write fail.
      process.stdin.destroy();
      lines.removeAllListeners('close');
      setTimeout(() => process.exit(0), 2000);
    }
    send({ jsonrpc: '2.0', id: frame.id, result: test.initialize });
  } else if (frame.method === 'initialized') {
    initialized = true;
  } else if (frame.method === 'session/start') {
    if (!initialized) throw new Error('Session request arrived before initialized');
    if (test.behavior === 'session-error') return send({ jsonrpc: '2.0', id: frame.id,
      error: { code: -32030, message: 'fixture start refused', data: { kind: 'commandRejected', reason: 'fixture' } } });
    send({ jsonrpc: '2.0', id: frame.id, result: test.opened });
  } else if (frame.method === 'skill/list') {
    send({ jsonrpc: '2.0', id: frame.id, result: test.skills });
  } else throw new Error(`Unexpected request ${frame.method}`);
});
