import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';

test('park rereading uses the latest three requests, waits for details to close and survives restarting', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180_000);
  const data = await mkdtemp('/private/tmp/ic-park-ui-');
  const model = path.join(data, 'model');
  const python = path.join(data, 'python');
  await mkdir(model);
  await writeFile(python, '#!/usr/bin/python3\n' + await readFile('tests/fixtures/park_editor.py', 'utf8'), { mode: 0o700 });
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await configureEditor(app, data, model, python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    for (let index = 0; index < 5; index++) {
      await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill(`park ${index}`);
      await page.getByRole('button', { name: 'Put', exact: true }).click();
      await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.text === `park ${index}`)?.decision?.kind).toBe('park');
    }
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('unlock the arithmetic task now');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.filter(word => word.text.startsWith('park ') && word.decision?.kind === 'extend').length).toBe(3);
    const state = await page.evaluate(() => window.intercat.getStatus());
    const remaining = state.utterances!.filter(word => word.decision?.kind === 'park');
    expect(remaining.map(word => word.text).sort()).toEqual(['park 0', 'park 1']);
    expect(remaining.every(word => word.state === 'placed')).toBe(true);
    const old = remaining.find(word => word.text === 'park 0')!;
    const chip = page.locator(`[data-utterance-id="${old.id}"]`);
    await expect(chip).toContainText('parked · say where');
    await expect(chip).toContainText('◇');
    await chip.click();
    const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(dialog).toContainText('Parked · choose a track to place it');
    await expect(dialog.getByRole('button', { name: 'Close request details', exact: true })).toBeEnabled();
    await app.evaluate(({ BrowserWindow }) => {
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2);
    });
    await expect(dialog.getByRole('button', { name: 'Close request details', exact: true })).toBeInViewport();
    expect((await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === old.id)!.decision!.kind).toBe('park');
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('park-200.png'), Buffer.from(pixels, 'base64'));
    await dialog.getByRole('button', { name: 'Close request details', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === old.id)!.decision!.kind).toBe('extend');
    await closeWindow(app);
    process.kill(pid!, 'SIGTERM');
    await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.filter(word => word.text.startsWith('park ') && word.decision?.kind === 'extend').length, { timeout: 95_000 }).toBe(4);
    const restored = await page.evaluate(() => window.intercat.getStatus());
    pid = restored.daemonPid;
    expect(restored.utterances!.find(word => word.id === old.id)!.reading!.state).toBe('complete');
    expect(restored.utterances!.find(word => word.text === 'park 1')!.decision!.kind).toBe('park');
    await writeFile(test.info().outputPath('park-restored.json'), JSON.stringify(restored, null, 2));
  } finally {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The test may have already stopped this isolated daemon. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    await rm(data, { recursive: true, force: true });
  }
});
