import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, access, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createHash, randomUUID } from 'node:crypto';
import path from 'node:path';
const execute = promisify(execFile);
const exists = async (file: string) => access(file).then(() => true, () => false);

test('saved project secret survives restart, reaches a new worker and is removed before later turns', async () => {
  test.skip(process.env.INTERCAT_TEST_PROJECT_SECRET_FLOW !== '1', 'Opt-in four paid dummy Muse turns');
  test.setTimeout(480000);
  const data = await mkdtemp('/private/tmp/ic-project-secret-ui-');
  const source = path.join(data, 'source'); await mkdir(source);
  const output = path.resolve('research/verify/secret-recovery-20261104/worker-native', randomUUID()); await mkdir(output, { recursive: true });
  const value = `dummy-project-token-${randomUUID()}`;
  const digest = createHash('sha256').update(value).digest('hex');
  await writeFile(path.join(source, 'verify_token.py'), `import hashlib,os\nfrom pathlib import Path\nvalue=os.environ.get('DEMO_TOKEN','')\nassert hashlib.sha256(value.encode()).hexdigest() == '${digest}', 'wrong test token'\nprint(value)\nPath('verified.txt').write_text('verified\\n')\nprint('verified')\n`);
  await writeFile(path.join(source, 'verify_absent.py'), `import os\nfrom pathlib import Path\nassert 'DEMO_TOKEN' not in os.environ, 'token remains'\nPath('absent.txt').write_text('absent\\n')\nprint('absent')\n`);
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  await git(['init', '-q', '-b', 'main']); await git(['add', '.']);
  await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture']);
  let app: ElectronApplication | undefined; let pid: number | undefined; let page: Page | undefined;
  const launch = async () => {
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 30000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
  };
  const shutdown = async () => {
    if (app) await closeWindow(app); app = undefined; page = undefined;
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return false; } catch { return true; } }, { timeout: 15000 }).toBe(true);
      pid = undefined;
    }
  };
  const completed = async (track: string, file: string) => {
    const approved = new Set<string>();
    await expect.poll(async () => {
      const state = await page!.evaluate(() => window.intercat.getStatus());
      const worker = state.workers?.find(worker => worker.trackId === track);
      for (const card of worker?.approvals ?? []) {
        if (approved.has(card.id)) continue; approved.add(card.id);
        expect(/^(?:python3|python|\/usr\/bin\/python3) verify_(?:token|absent)\.py$/u.test(card.subject)).toBe(true);
        await page!.evaluate(async ({ worker, card }) => window.intercat.answerWorker({ opId: await window.intercat.createOperationId(), trackId: worker.trackId, approvalId: card.id, requirement: card.requirement, allow: true }), { worker: worker!, card });
      }
      if (['failed', 'lost', 'needs-recovery'].includes(worker?.phase ?? '')) throw new Error(`Worker failed: ${worker?.detail}`);
      const workspace = state.workspaces?.find(workspace => workspace.trackId === track);
      return !!workspace && await exists(path.join(workspace.path, file)) && worker?.activeTurn === null && worker?.checkpointPending === false && !!worker?.checkpoint;
    }, { timeout: 120000 }).toBe(true);
  };
  try {
    await launch();
    await app!.evaluate(({ dialog }, paths) => { const picks = paths.map(file => ({ canceled: false, filePaths: [file] })); dialog.showOpenDialog = async () => picks.shift()!; }, [source, process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page!.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await configureEditor(app!, data);
    await expect(page!.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    await page!.getByRole('textbox', { name: 'Request draft' }).fill('Start now. Do not inspect files or run tools yet. You need DEMO_TOKEN in the environment before running python3 verify_token.py. End your final answer with this exact standalone line: Waiting for you: Please provide DEMO_TOKEN so I can run python3 verify_token.py. After the token is provided, run that script once, report its output, and finish without asking another question.');
    await page!.getByRole('button', { name: 'New track', exact: true }).click();
    await page!.getByRole('button', { name: 'Connect workers', exact: true }).click();
    const question = page!.getByRole('group', { name: 'Question from Worker A', exact: true });
    await expect(question).toContainText('DEMO_TOKEN', { timeout: 120000 });
    await expect.poll(async () => { const worker = (await page!.evaluate(() => window.intercat.getStatus())).workers?.[0]; return worker?.activeTurn === null && worker.checkpointPending === false && !!worker.checkpoint; }).toBe(true);
    const first = await page!.evaluate(() => window.intercat.getStatus());
    const trackA = first.workers![0].trackId;
    await question.getByRole('button', { name: 'Provide a secret', exact: true }).click();
    await expect(question.getByRole('radio', { name: 'Save for this project', exact: true })).toBeChecked();
    await question.getByLabel('Secret variable name', { exact: true }).fill('DEMO_TOKEN');
    await question.getByLabel('Secret value', { exact: true }).fill(value);
    await page!.screenshot({ path: path.join(output, 'save-form.png') });
    await question.getByRole('button', { name: 'Save and use', exact: true }).click();
    await completed(trackA, 'verified.txt');
    const saved = await page!.evaluate(() => window.intercat.getStatus());
    expect(saved.projectSecrets?.names).toEqual(['DEMO_TOKEN']);
    expect(saved.workers!.find(worker => worker.trackId === trackA)!.sessionId).toBe(first.workers![0].sessionId);
    await shutdown(); await launch();
    await expect(page!.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    await expect.poll(async () => (await page!.evaluate(() => window.intercat.getStatus())).projectSecrets?.ready).toBe(true);
    const reloaded = await page!.evaluate(() => window.intercat.getStatus());
    expect(reloaded.projectSecrets?.names).toEqual(['DEMO_TOKEN']);
    await page!.getByRole('textbox', { name: 'Request draft' }).fill('Run python3 verify_token.py exactly once. The required environment is already configured. Report its output and finish. Do not ask a question or run anything else.');
    await page!.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page!.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(2);
    const trackB = (await page!.evaluate(() => window.intercat.getStatus())).tracks!.find(track => track.id !== trackA)!.id;
    await completed(trackB, 'verified.txt');
    const beforeRemove = await page!.evaluate(() => window.intercat.getStatus());
    await page!.getByRole('button', { name: 'Settings', exact: true }).click();
    const settings = page!.getByRole('region', { name: 'Project secrets', exact: true });
    await expect(settings).toContainText('DEMO_TOKEN');
    await settings.getByRole('button', { name: 'Remove DEMO_TOKEN', exact: true }).click();
    await expect(settings.getByRole('button', { name: 'Remove DEMO_TOKEN', exact: true })).toHaveCount(0);
    await page!.screenshot({ path: path.join(output, 'removed.png') });
    await page!.getByRole('button', { name: 'Close settings', exact: true }).click();
    await expect.poll(async () => {
      const state = await page!.evaluate(() => window.intercat.getStatus());
      return state.projectSecrets?.names.length === 0 && [trackA, trackB].every(track => {
        const before = beforeRemove.workers!.find(worker => worker.trackId === track);
        const after = state.workers!.find(worker => worker.trackId === track);
        return !!after?.identity && before?.identity?.pid !== after.identity.pid && before?.sessionId === after.sessionId;
      });
    }, { timeout: 30000 }).toBe(true);
    await page!.getByRole('textbox', { name: 'Request draft' }).fill('Run python3 verify_absent.py exactly once, report its output and finish. Do not ask a question or run anything else.');
    await page!.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page!.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(3);
    const trackC = (await page!.evaluate(() => window.intercat.getStatus())).tracks!.find(track => ![trackA, trackB].includes(track.id))!.id;
    await completed(trackC, 'absent.txt');
    const last = await page!.evaluate(() => window.intercat.getStatus());
    expect(last.projectSecrets?.names).toEqual([]);
    expect(JSON.stringify(last).includes(value)).toBe(false);
    expect((await page!.content()).includes(value)).toBe(false);
    expect(await exists(path.join(source, 'verified.txt'))).toBe(false);
    expect(await exists(path.join(source, 'absent.txt'))).toBe(false);
    await page!.screenshot({ path: path.join(output, 'completed.png') });
    await writeFile(path.join(output, 'result.json'), JSON.stringify({ savedToProject: true, defaultScopeProject: true, survivedAppAndDaemonRestart: true, newWorkerReceivedValue: true, removedBeforeNewWorker: true, existingHostsRefreshedInSameSession: true, originalUnchanged: true, stateHasNoRawValue: true, first, saved, reloaded, beforeRemove, last }, null, 2));
  } finally {
    if (page) {
      const state = await page.evaluate(() => window.intercat.getStatus()).catch(() => null);
      if (state?.projectSecrets?.names.includes('DEMO_TOKEN')) await page.evaluate(async revision => window.intercat.removeProjectSecret({ opId: await window.intercat.createOperationId(), name: 'DEMO_TOKEN', revision }), state.projectSecrets.revision);
    }
    await shutdown();
    const database = path.join(data, 'daemon/events.sqlite');
    if (await exists(database)) {
      const counts = await execute('/usr/bin/python3', ['-c', "import sqlite3,json,sys; d=sqlite3.connect(sys.argv[1]); print(json.dumps({'attemptedModelTurns':d.execute(\"select count(*) from operations where kind='turn.start'\").fetchone()[0], 'fingerprintRevision':d.execute('select revision from secret_catalog').fetchone()[0], 'catalogSealed':bool(d.execute('select revision=sealed_revision and digest=sealed_digest from secret_catalog').fetchone()[0])}))", database]);
      await writeFile(path.join(output, 'attempt.json'), counts.stdout);
      const archive = path.join(output, 'events.sqlite');
      await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; source=sqlite3.connect(sys.argv[1]); target=sqlite3.connect(sys.argv[2]); source.backup(target); target.close(); source.close()', database, archive]);
      const bytes = await readFile(archive); expect(bytes.includes(Buffer.from(value))).toBe(false);
    }
    await rm(data, { recursive: true, force: true });
  }
});
