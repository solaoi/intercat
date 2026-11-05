import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, access, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createHash, randomUUID } from 'node:crypto';
import path from 'node:path';
const execute = promisify(execFile);
const exists = async (file: string) => access(file).then(() => true, () => false);

test('real UI provides a one-time secret through Keychain receipts and restarts the worker', async () => {
  test.skip(process.env.INTERCAT_TEST_SECRET_FLOW !== '1', 'Opt-in two paid dummy Muse turns');
  test.setTimeout(300000);
  const data = await mkdtemp('/private/tmp/ic-secret-ui-');
  const source = path.join(data, 'source'); await mkdir(source);
  const output = path.resolve('research/verify/report-secret-20261104/ui-native', randomUUID()); await mkdir(output, { recursive: true });
  const value = `dummy-ui-token-${randomUUID()}`;
  const digest = createHash('sha256').update(value).digest('hex');
  await writeFile(path.join(source, 'verify_token.py'), `import hashlib,os\nfrom pathlib import Path\nvalue=os.environ.get('DEMO_TOKEN','')\nassert hashlib.sha256(value.encode()).hexdigest() == '${digest}', 'wrong test token'\nprint(value)\nPath('verified.txt').write_text('verified\\n')\nprint('verified')\n`);
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  await git(['init', '-q', '-b', 'main']); await git(['add', 'verify_token.py']);
  await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture']);
  const app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] }); let pid: number | undefined;
  try {
    const page = await app.firstWindow(); await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => { const picks = paths.map(file => ({ canceled: false, filePaths: [file] })); dialog.showOpenDialog = async () => picks.shift()!; }, [source, process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await configureEditor(app, data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Start now. Do not inspect files or run tools yet. You need DEMO_TOKEN in the environment before running python3 verify_token.py. End your final answer with this exact standalone line: Waiting for you: Please provide DEMO_TOKEN so I can run python3 verify_token.py. After the token is provided, run that script once, report its output, and finish without asking another question.');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
    const question = page.getByRole('group', { name: 'Question from Worker A', exact: true });
    await expect(question).toContainText('DEMO_TOKEN', { timeout: 120000 });
    await expect.poll(async () => { const worker = (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]; return worker?.activeTurn === null && worker.checkpointPending === false && !!worker.checkpoint; }).toBe(true);
    const first = await page.evaluate(() => window.intercat.getStatus());
    const workspace = first.workspaces![0].path;
    await question.getByRole('button', { name: 'Provide a secret', exact: true }).click();
    await question.getByRole('radio', { name: 'This time only', exact: true }).check();
    await question.getByLabel('Secret variable name', { exact: true }).fill('DEMO_TOKEN');
    await question.getByLabel('Secret value', { exact: true }).fill(value);
    await page.screenshot({ path: path.join(output, 'form.png') });
    await question.getByRole('button', { name: 'Use for this worker', exact: true }).click();
    const approved = new Set<string>();
    await expect.poll(async () => {
      const state = await page.evaluate(() => window.intercat.getStatus()); const worker = state.workers?.[0];
      for (const card of worker?.approvals ?? []) {
        if (approved.has(card.id)) continue; approved.add(card.id);
        expect(['python3 verify_token.py', 'python verify_token.py', '/usr/bin/python3 verify_token.py']).toContain(card.subject);
        await page.evaluate(async ({ worker, card }) => window.intercat.answerWorker({ opId: await window.intercat.createOperationId(), trackId: worker.trackId, approvalId: card.id, requirement: card.requirement, allow: true }), { worker: worker!, card });
      }
      if (['failed', 'lost', 'needs-recovery'].includes(worker?.phase ?? '')) throw new Error(`Worker failed: ${worker?.detail}`);
      return await exists(path.join(workspace, 'verified.txt')) && worker?.activeTurn === null && worker?.checkpointPending === false && worker?.checkpoint?.turnId !== first.workers![0].checkpoint?.turnId;
    }, { timeout: 120000 }).toBe(true);
    const last = await page.evaluate(() => window.intercat.getStatus());
    expect(last.workers![0].sessionId).toBe(first.workers![0].sessionId);
    expect(last.workers![0].identity?.pid).not.toBe(first.workers![0].identity?.pid);
    expect((await readFile(path.join(workspace, 'verified.txt'), 'utf8')).trim()).toBe('verified');
    expect(await exists(path.join(source, 'verified.txt'))).toBe(false);
    expect(JSON.stringify(last).includes(value)).toBe(false);
    expect((await page.content()).includes(value)).toBe(false);
    await page.screenshot({ path: path.join(output, 'completed.png') });
    await writeFile(path.join(output, 'result.json'), JSON.stringify({ sameSession: true, newHost: true, keychainReceiptConfirmed: true, verifiedFile: true, originalUnchanged: true, stateHasNoRawValue: true, first, last }, null, 2));
  } finally {
    await closeWindow(app);
    if (pid) { process.kill(pid, 'SIGTERM'); await expect.poll(() => { try { process.kill(pid!, 0); return false; } catch { return true; } }, { timeout: 15000 }).toBe(true); }
    const database = path.join(data, 'daemon/events.sqlite');
    if (await exists(database)) {
      const counts = await execute('/usr/bin/python3', ['-c', "import sqlite3,json,sys; d=sqlite3.connect(sys.argv[1]); print(json.dumps({'attemptedModelTurns':d.execute(\"select count(*) from operations where kind='turn.start'\").fetchone()[0], 'fingerprintRevision':d.execute('select revision from secret_catalog').fetchone()[0], 'catalogSealed':bool(d.execute('select revision=sealed_revision and digest=sealed_digest from secret_catalog').fetchone()[0])}))", database]);
      await writeFile(path.join(output, 'attempt.json'), counts.stdout);
      expect(JSON.parse(counts.stdout).catalogSealed).toBe(true);
      const bytes = await readFile(database);
      expect(bytes.includes(Buffer.from(value))).toBe(false);
      await writeFile(path.join(output, 'events.sqlite'), bytes);
    }
    await rm(data, { recursive: true, force: true });
  }
});
