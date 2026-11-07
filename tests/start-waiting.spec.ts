import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

test('go-ahead uses the stored choice after restart and automatically starts a sole waiting request', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(240_000);
  const data = await mkdtemp('/private/tmp/ic-start-waiting-');
  const model = path.join(data, 'model');
  const python = path.join(data, 'python');
  await mkdir(model);
  await writeFile(python, '#!/usr/bin/python3\n' + await readFile('tests/fixtures/park_editor.py', 'utf8'), { mode: 0o700 });
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  const stop = async () => {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already exited. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
      pid = undefined;
    }
  };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await configureEditor(app, data, model, python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    for (const text of ['When I say, prepare the first change', 'When I say, prepare the second change']) {
      await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill(text);
      await page.getByRole('button', { name: 'New track', exact: true }).click();
      await expect(page.locator('.request-chip').filter({ hasText: text })).toContainText('waiting for your go');
    }
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Start waiting work');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect(page.getByRole('group', { name: 'Start waiting work', exact: true })).toContainText('Which waiting request should start?');
    await stop();
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    const card = page.getByRole('group', { name: 'Start waiting work', exact: true });
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      const choice = card.getByRole('button', { name: /Start Worker B/ });
      await choice.focus();
      await expect(choice).toBeInViewport();
      await expect(choice).toBeEnabled();
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`waiting-choice-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
    }
    await card.getByRole('button', { name: /Start Worker B/ }).click();
    await expect(card).toHaveCount(0);
    let status = await page.evaluate(() => window.intercat.getStatus());
    expect(status.utterances!.find(word => word.text === 'When I say, prepare the second change')!.decision!.boundary).toBe('now');
    expect(status.utterances!.find(word => word.text === 'When I say, prepare the first change')!.decision!.boundary).toBe('when-told');
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Start waiting work again');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.text === 'Start waiting work again')?.state).toBe('landed');
    status = await page.evaluate(() => window.intercat.getStatus());
    expect(status.tracks).toHaveLength(2);
    expect(status.utterances!.filter(word => word.decision?.boundary === 'when-told')).toHaveLength(0);
    expect(status.utterances!.filter(word => word.decision?.kind === 'command' && word.state === 'landed')).toHaveLength(2);
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Start waiting work once more');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect(card).toContainText('Nothing is waiting for your go.');
    await expect(card.locator('.waiting-requests button')).toHaveCount(0);
    await writeFile(test.info().outputPath('commands.json'), JSON.stringify(await page.evaluate(() => window.intercat.getStatus()), null, 2));
  } finally {
    await stop();
    await rm(data, { recursive: true, force: true });
  }
});
