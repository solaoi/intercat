import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { timelineClips, timelineMarks } from './support/timeline';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';

const execute = promisify(execFile);
test('managed workers edit a fixture, stop live work and hold a push for approval', async () => {
  test.skip(process.env.INTERCAT_TEST_RUN_WORKER !== '1', 'Opt-in paid dummy worker turns');
  test.setTimeout(240000);
  for (const name of ['INTERCAT_TEST_EDITOR_MODEL', 'INTERCAT_TEST_EDITOR_PYTHON', 'INTERCAT_TEST_MUSE_BINARY']) expect(process.env[name], name).toBeTruthy();
  const data = await mkdtemp('/private/tmp/ic-execution-ui-');
  const source = path.join(data, 'source'); const remote = path.join(data, 'remote.git');
  await mkdir(source);
  const environment = { PATH: process.env.PATH, HOME: data, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env: environment });
  await git(['init', '-b', 'main']);
  await writeFile(path.join(source, 'math.cjs'), 'exports.add = (a, b) => a - b;\n');
  await writeFile(path.join(source, 'math.test.cjs'), "const test = require('node:test'); const assert = require('node:assert/strict'); const { add } = require('./math.cjs'); test('addition', () => assert.equal(add(2, 3), 5));\n");
  await git(['add', '.']); await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'dummy arithmetic']);
  await execute('/usr/bin/git', ['init', '--bare', remote], { env: environment });
  await git(['remote', 'add', 'origin', remote]);
  const app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  let pid: number | undefined;
  try {
    const page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => { const picks = paths.map(file => ({ canceled: false, filePaths: [file] })); dialog.showOpenDialog = async () => picks.shift()!; }, [source, process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await configureEditor(app, data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    const first = 'Start now. Fix the add function in math.cjs, which subtracts by mistake. Run node --test math.test.cjs, and report the actual result. Change only math.cjs.';
    await page.getByRole('textbox', { name: 'Request draft' }).fill(first);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.sha, { timeout: 120000 }).toBeTruthy();
    const fixed = await page.evaluate(() => window.intercat.getStatus());
    expect(fixed.workers![0].output).toBeTruthy();
    const working = fixed.workspaces![0].path;
    await execute('node', ['--test', 'math.test.cjs'], { cwd: working, env: environment });
    expect(await readFile(path.join(source, 'math.cjs'), 'utf8')).toBe('exports.add = (a, b) => a - b;\n');
    const editStep = (await timelineClips(page)).find(clip => clip.kind === 'edit');
    expect(editStep).toBeTruthy();
    expect((await timelineMarks(page))[0].text).toBe(first);
    expect(fixed.utterances?.[0].state).toBe('landed');
    const detail = await page.evaluate(id => window.intercat.getClipDetail(id), editStep!.id);
    const patch = detail.items.find(item => item.outputs.patchRef)?.outputs.patchRef;
    expect(patch?.encoding).toBe('utf8');
    expect(patch?.content).toContain('+exports.add = (a, b) => a + b;');
    const stepButton = page.getByRole('button', { name: `Open step A:${editStep!.ordinal}: ${editStep!.label}`, exact: true });
    await stepButton.focus(); await stepButton.press('Enter');
    await expect(page.getByLabel('Step details')).toContainText('+exports.add = (a, b) => a + b;');
    await expect(page.locator('.step-artifact pre').first()).toBeInViewport();
    await page.screenshot({ path: 'research/verify/execution-ui-20261104/timeline.png' });
    await writeFile('research/verify/execution-ui-20261104/timeline.json', JSON.stringify({ timeline: fixed.timeline, detail }, null, 2));
    await page.getByRole('button', { name: 'Close step preview' }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).reports?.length ?? 0, { timeout: 15000 }).toBe(0);
    for (const question of (await page.evaluate(() => window.intercat.getStatus())).reportQuestions ?? []) {
      expect(question.kind).toBe('review');
      await page.evaluate(async q => window.intercat.respondToReport({ opId: await window.intercat.createOperationId(), questionId: q.id, revision: q.revision, answer: { action: 'continue' } }), question);
    }
    const firstCheckpoint = fixed.workers![0].checkpoint!;
    await page.evaluate(async trackId => { await window.intercat.stageUtterance({ attachmentIds: [], opId: await window.intercat.createOperationId(), text: 'Start now. Create proof.txt containing exactly done on one line. Do not change other files. Report the result.', target: { mode: 'lock', trackId }, selectedTrackId: trackId }); }, fixed.workers![0].trackId);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.turnId, { timeout: 60000 }).not.toBe(firstCheckpoint.turnId);
    const continued = (await page.evaluate(() => window.intercat.getStatus())).workers![0];
    expect(continued.sessionId).toBe(fixed.workers![0].sessionId);
    expect((await git(['rev-parse', `${continued.checkpoint!.sha}^`])).stdout.trim()).toBe(firstCheckpoint.sha);
    expect((await git(['show', `${continued.checkpoint!.sha}:proof.txt`])).stdout.trim()).toBe('done');
    const second = "Start now. Use bash to run exactly this command once and wait for it to finish: /usr/bin/python3 -c 'from pathlib import Path; import os, time; Path(\"ready-to-stop\").write_text(str(os.getpid())); time.sleep(60)'. Do not run anything else or detach the process.";
    await page.getByRole('textbox', { name: 'Request draft' }).fill(second);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workspaces?.length, { timeout: 10000 }).toBe(2);
    const secondPath = (await page.evaluate(() => window.intercat.getStatus())).workspaces![1].path;
    await expect.poll(async () => { try { return Number(await readFile(path.join(secondPath, 'ready-to-stop'), 'utf8')) > 0; } catch { return false; } }, { timeout: 60000 }).toBe(true);
    const childPid = Number(await readFile(path.join(secondPath, 'ready-to-stop'), 'utf8'));
    await page.getByRole('button', { name: 'Stop Worker B', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[1]?.detail, { timeout: 15000 }).toContain('cancelled');
    await expect.poll(() => { try { process.kill(childPid, 0); return true; } catch { return false; } }, { timeout: 5000 }).toBe(false);
    const third = 'Start now. Use bash to run exactly git push origin HEAD:refs/heads/probe once. Do not run other commands, retry it, or edit any files. Report the result.';
    await page.getByRole('textbox', { name: 'Request draft' }).fill(third);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect(page.getByRole('group', { name: 'Approval for Worker C' })).toBeVisible({ timeout: 60000 });
    expect((await execute('/usr/bin/git', ['--git-dir', remote, 'for-each-ref', '--format=%(refname)'], { env: environment })).stdout.trim()).toBe('');
    await page.screenshot({ path: 'research/verify/execution-ui-20261104/approval.png' });
    await page.getByRole('button', { name: 'Don’t run it', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[2]?.checkpoint?.sha, { timeout: 60000 }).toBeTruthy();
    expect((await execute('/usr/bin/git', ['--git-dir', remote, 'for-each-ref', '--format=%(refname)'], { env: environment })).stdout.trim()).toBe('');
  } finally {
    await mkdir('research/verify/execution-ui-20261104', { recursive: true });
    try { await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); d=sqlite3.connect(sys.argv[2]); s.backup(d); d.close(); s.close()', path.join(data, 'daemon/events.sqlite'), path.resolve('research/verify/execution-ui-20261104/events.sqlite')]); } catch { /* Preserve the original failure if setup did not reach SQLite. */ }
    await closeWindow(app);
    if (pid) { process.kill(pid, 'SIGTERM'); await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false); }
    await rm(data, { recursive: true, force: true });
  }
});
