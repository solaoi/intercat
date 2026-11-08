import { closeWindow } from './support/close';
import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import path from 'node:path';

const execute = promisify(execFile);
test('choosing a project rejects local changes and persists the clean starting commit', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon integration');
  const data = await mkdtemp('/private/tmp/ic-project-ui-');
  const repo = path.join(data, 'source');
  await mkdir(repo);
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', repo, ...args], { env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  await git(['init', '-b', 'main']);
  await writeFile(path.join(repo, 'test.txt'), 'committed\n');
  await git(['add', '.']);
  await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'Initial test']);
  const head = (await git(['rev-parse', 'HEAD'])).stdout.trim();
  const originalIndex = await readFile(path.join(repo, '.git/index'));
  await writeFile(path.join(repo, 'test.txt'), 'uncommitted\n');
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  let pid: number | undefined;
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, repo);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('uncommitted changes');
    expect((await page.evaluate(() => window.intercat.getStatus())).project).toBeNull();
    expect(await readFile(path.join(repo, 'test.txt'), 'utf8')).toBe('uncommitted\n');
    await writeFile(path.join(repo, 'test.txt'), 'committed\n');
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).project?.folders[0].base).toBe(head);
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Keep the selected starting point.');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).session?.bound).toBe(true);
    const original = await page.evaluate(() => window.intercat.getStatus());
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.locator('.session-subtitle')).toContainText('source');
    const restored = await page.evaluate(() => window.intercat.getStatus());
    expect(restored.project).toEqual(original.project);
    expect(restored.session).toEqual(original.session);
    expect(restored.daemonPid).toBe(pid);
    expect(await readFile(path.join(repo, '.git/index'))).toEqual(originalIndex);
    expect((await git(['rev-parse', 'HEAD'])).stdout.trim()).toBe(head);
    await mkdir('research/verify/project-ui-20261104', { recursive: true });
    await page.screenshot({ path: 'research/verify/project-ui-20261104/selected.png' });
  } finally {
    await closeWindow(app);
    if (pid) process.kill(pid, 'SIGTERM');
    await rm(data, { recursive: true, force: true });
  }
});
