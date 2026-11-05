import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { timelineClips } from './support/timeline';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
const execute = promisify(execFile);

test('a same-turn deletion is saved before approval and restored from its step', async () => {
  test.skip(process.env.INTERCAT_TEST_RUN_WORKER !== '1', 'Opt-in paid dummy worker turn');
  test.setTimeout(180000);
  const data = await mkdtemp('/private/tmp/ic-deletion-ui-');
  const source = path.join(data, 'source');
  const evidence = 'research/verify/deletion-ui-20261104';
  await mkdir(source); await mkdir(evidence, { recursive: true });
  const env = { PATH: process.env.PATH, HOME: data, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  const git = (args: string[], cwd = source) => execute('/usr/bin/git', args, { cwd, env });
  await git(['init', '-b', 'main']); await writeFile(path.join(source, 'README.txt'), 'Disposable deletion fixture.\n');
  await git(['add', '.']); await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'fixture']);
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
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Start now. Use write_file to create fleeting.txt containing exactly snapshot-value on one line. Then use bash to run exactly rm -- fleeting.txt once. Do not use another deletion mechanism, edit any other file, or ask for user input. Report the actual result.');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.sha, { timeout: 100000 }).toBeTruthy();
    const state = await page.evaluate(() => window.intercat.getStatus());
    const workspace = state.workspaces![0].path;
    await expect(readFile(path.join(workspace, 'fleeting.txt'))).rejects.toThrow();
    const details = await page.evaluate(async clips => Promise.all(clips.map(clip => window.intercat.getClipDetail(clip.id))), (await timelineClips(page)));
    const step = details.find(detail => detail.deletionSnapshots?.some(snapshot => snapshot.purpose === 'before-delete'))!;
    expect(step).toBeTruthy();
    const saved = step.deletionSnapshots!.find(snapshot => snapshot.purpose === 'before-delete')!;
    expect(saved.complete).toBe(true); expect(saved.paths).toContain('fleeting.txt');
    expect((await git(['show', `${saved.sha}:fleeting.txt`], workspace)).stdout.trim()).toBe('snapshot-value');
    const indexPath = (await git(['rev-parse', '--git-path', 'index'], workspace)).stdout.trim();
    const index = await readFile(indexPath); const head = (await git(['rev-parse', 'HEAD'], workspace)).stdout;
    const button = page.getByRole('button', { name: `Open step A:${step.clip.ordinal}: ${step.clip.label}`, exact: true });
    await button.focus(); await button.press('Enter');
    const recovery = page.getByRole('region', { name: 'Before deletion', exact: true });
    await recovery.getByRole('button', { name: 'Restore saved files', exact: true }).click();
    await recovery.getByRole('button', { name: 'Restore now', exact: true }).click();
    await expect(recovery).toContainText('Restored 1 saved files.', { timeout: 30000 });
    expect(await readFile(path.join(workspace, 'fleeting.txt'), 'utf8')).toBe('snapshot-value\n');
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.phase).toBe('held');
    expect(await readFile(indexPath)).toEqual(index); expect((await git(['rev-parse', 'HEAD'], workspace)).stdout).toBe(head);
    await expect(readFile(path.join(source, 'fleeting.txt'))).rejects.toThrow();
    await page.screenshot({ path: `${evidence}/restored.png` });
    const undo = page.getByRole('region', { name: 'Before restore', exact: true }).first();
    await undo.getByRole('button', { name: 'Restore saved files', exact: true }).click();
    await undo.getByRole('button', { name: 'Restore now', exact: true }).click();
    await expect(undo).toContainText('Restored 0 saved files.', { timeout: 30000 });
    await expect.poll(async () => { try { await readFile(path.join(workspace, 'fleeting.txt')); return false; } catch { return true; } }).toBe(true);
    await writeFile(`${evidence}/result.json`, JSON.stringify({ saved, originalCheckoutUnchanged: true, indexUnchanged: true, headUnchanged: true, restored: true, undoRestored: true }, null, 2));
  } finally {
    try { await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); d=sqlite3.connect(sys.argv[2]); s.backup(d); d.close(); s.close()', path.join(data, 'daemon/events.sqlite'), path.resolve(`${evidence}/events.sqlite`)]); } catch { /* Preserve the original test error. */ }
    await closeWindow(app);
    if (pid) { process.kill(pid, 'SIGTERM'); await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false); }
    await rm(data, { recursive: true, force: true });
  }
});
