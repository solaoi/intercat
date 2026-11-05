import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, chmod, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { recoveryCommand } from './recovery-command.mjs';

test('private recovery sends its bootstrap only through stdin and leaves stderr out of the result', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'intercat-private-recovery-'));
  try {
    await chmod(root, 0o700);
    const binary = path.join(root, 'fixture.mjs');
    await writeFile(binary, `#!${process.execPath}\nlet text='';for await(const chunk of process.stdin)text+=chunk;const input=JSON.parse(text);process.stderr.write('private diagnostic');console.log(JSON.stringify({hasKey:input.secretKey==='dummy-bootstrap',args:process.argv.slice(2),environmentHasKey:Object.values(process.env).includes('dummy-bootstrap')}));\n`, { mode: 0o700 });
    const result = await recoveryCommand(binary, root, 'review-backup', { secretKey: 'dummy-bootstrap', request: { action: 'inspect' } });
    assert.deepEqual(result, { hasKey: true, args: ['review-backup', root], environmentHasKey: false });
    await chmod(root, 0o755);
    await assert.rejects(recoveryCommand(binary, root, 'review-backup', {}), /Unsafe daemon directory/);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test('private recovery rejects an oversized response without returning partial output', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'intercat-private-recovery-'));
  try {
    await chmod(root, 0o700);
    const binary = path.join(root, 'fixture.mjs');
    await writeFile(binary, `#!${process.execPath}\nprocess.stdin.resume();process.stdout.write('x'.repeat(2*1024*1024));setTimeout(()=>{},60000);\n`, { mode: 0o700 });
    await assert.rejects(recoveryCommand(binary, root, 'review-backup', {}), /did not finish/);
  } finally { await rm(root, { recursive: true, force: true }); }
});
