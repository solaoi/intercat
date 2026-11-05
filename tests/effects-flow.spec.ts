import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { timelineClips } from './support/timeline';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
const execute = promisify(execFile);

test('an approved push to a disposable local remote is recorded as an external effect', async () => {
  test.skip(process.env.INTERCAT_TEST_RUN_WORKER !== '1', 'Opt-in paid dummy worker turn');
  test.setTimeout(180000);
  const data = await mkdtemp('/private/tmp/ic-effects-ui-');
  const source = path.join(data, 'source'); const remote = path.join(data, 'remote.git');
  const evidence = 'research/verify/effects-ui-20261104';
  await mkdir(source); await mkdir(evidence, { recursive: true });
  const env = { PATH: process.env.PATH, HOME: data, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env });
  await git(['init', '-b', 'main']); await writeFile(path.join(source, 'README.txt'), 'Disposable local push fixture.\n');
  await git(['add', '.']); await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'fixture']);
  const base = (await git(['rev-parse', 'HEAD'])).stdout.trim();
  await execute('/usr/bin/git', ['init', '--bare', remote], { env }); await git(['remote', 'add', 'origin', remote]);
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  let pid: number | undefined;
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => { const picks = paths.map(file => ({ canceled: false, filePaths: [file] })); dialog.showOpenDialog = async () => picks.shift()!; }, [source, process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click(); await configureEditor(app, data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Start now. Use bash to run exactly git push origin HEAD:refs/heads/effect-probe once. Do not edit files, run other commands, or retry. Report the actual result. The remote is a disposable local fixture.');
    await page.getByRole('button', { name: 'New track', exact: true }).click(); await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
    const approval = page.getByRole('group', { name: 'Approval for Worker A' });
    await expect(approval).toBeVisible({ timeout: 100000 });
    expect((await page.evaluate(() => window.intercat.getStatus())).effects).toEqual([]);
    expect((await execute('/usr/bin/git', ['--git-dir', remote, 'for-each-ref', '--format=%(refname)'], { env })).stdout.trim()).toBe('');
    await approval.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.sha, { timeout: 60000 }).toBeTruthy();
    const state = await page.evaluate(() => window.intercat.getStatus());
    expect(state.effects).toHaveLength(1); const effect = state.effects![0];
    expect(effect).toMatchObject({ kind: 'push', target: 'origin', outcome: 'done', running: false, turnId: state.workers![0].checkpoint!.turnId });
    expect((await execute('/usr/bin/git', ['--git-dir', remote, 'rev-parse', 'refs/heads/effect-probe'], { env })).stdout.trim()).toBe(base);
    await expect(page.getByRole('region', { name: 'Effects outside Git', exact: true })).toContainText('Restoring files does not undo these actions.');
    const clip = (await timelineClips(page)).find(clip => clip.id === effect.clipId)!;
    expect(clip.effectCount).toBe(1);
    const button = page.getByRole('button', { name: `Open step A:${clip.ordinal}: ${clip.label}`, exact: true });
    await button.focus(); await button.press('Enter');
    await expect(page.getByLabel('Step details').getByRole('region', { name: 'Effects outside Git', exact: true })).toContainText('Push · origin');
    await page.screenshot({ path: `${evidence}/completed.png` });
    await writeFile(`${evidence}/result.json`, JSON.stringify({ effect, clip, localRemoteTarget: base, approvalBeforeExecution: true }, null, 2));
    await closeWindow(app); app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] }); page = await app.firstWindow();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).effects?.[0]?.id).toBe(effect.id);
  } finally {
    try { await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); d=sqlite3.connect(sys.argv[2]); s.backup(d); d.close(); s.close()', path.join(data, 'daemon/events.sqlite'), path.resolve(`${evidence}/events.sqlite`)]); } catch { /* Preserve the original failure. */ }
    await closeWindow(app);
    if (pid) { process.kill(pid, 'SIGTERM'); await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false); }
    await rm(data, { recursive: true, force: true });
  }
});
