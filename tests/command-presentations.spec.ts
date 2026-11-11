import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

test('application actions open the requested view and only record display, including after a restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(240_000);
  const data = await mkdtemp('/private/tmp/ic-command-presentations-');
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
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Open settings');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    let settings = page.getByRole('dialog', { name: 'Settings · App', exact: true });
    await expect(settings).toBeVisible();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.text === 'Open settings')?.state).toBe('landed');
    await settings.getByRole('button', { name: 'Close settings', exact: true }).click();
    // Keep the next request pending behind an existing dialog, then restart both processes.
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    const queued = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Open commit', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === queued.id)?.presentation?.command).toBe('commit');
    const pending = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === queued.id)!;
    expect(pending.state).toBe('placed');
    await expect(page.getByLabel('Commit review', { exact: true })).toHaveCount(0);
    await stop();
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    settings = page.getByRole('dialog', { name: 'Settings · App', exact: true });
    const review = page.getByLabel('Commit review', { exact: true });
    await expect(review).toBeVisible();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === queued.id)?.state).toBe('landed');
    // This fixture has no project. Opening its review must remain separate from creating a commit.
    expect((await page.evaluate(() => window.intercat.getStatus())).integrations![0]).toBeNull();
    await review.getByRole('button', { name: 'Hide commit review', exact: true }).click();
    await expect(review).toHaveCount(0);
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    const heldRequest = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Open commit after hold', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === heldRequest.id)?.presentation?.command).toBe('commit');
    await page.evaluate(async id => {
      const word = (await window.intercat.getStatus()).utterances!.find(word => word.id === id)!;
      await window.intercat.resolveUtterance({ opId: await window.intercat.createOperationId(), utteranceId: id, revision: word.revision, resolution: { action: 'hold' } });
    }, heldRequest.id);
    await settings.getByRole('button', { name: 'Close settings', exact: true }).click();
    await expect(review).toHaveCount(0);
    const heldReading = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === heldRequest.id)!.reading?.index;
    await page.getByRole('button', { name: 'Start request: Open commit after hold', exact: true }).click();
    await expect(review).toBeVisible();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === heldRequest.id)?.state).toBe('landed');
    expect((await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.id === heldRequest.id)!.reading?.index).toBe(heldReading);
    await review.getByRole('button', { name: 'Hide commit review', exact: true }).click();
    await page.reload();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    await expect(review).toHaveCount(0);
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Stage mode');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect(page.locator('.app')).toHaveClass(/stage-mode/);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.text === 'Stage mode')?.state).toBe('landed');
    await page.keyboard.press('Meta+Shift+T');
    await expect(page.locator('.app')).not.toHaveClass(/stage-mode/);
    await writeFile(test.info().outputPath('presented.json'), JSON.stringify(await page.evaluate(() => window.intercat.getStatus()), null, 2));
  } finally {
    await stop();
    await rm(data, { recursive: true, force: true });
  }
});

test('a failed display receipt retries the same operation without changing settings', async () => {
  const data = await mkdtemp('/private/tmp/ic-command-receipt-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  try {
    await app.evaluate(({ ipcMain }) => {
      const word = { id: 'command', text: 'Open settings', state: 'placed', revision: 1, createdAt: 1, target: { mode: 'none' }, attachments: [], reason: null,
        decision: { kind: 'command', command: 'settings', trackId: null, boundary: null },
        presentation: { command: 'settings', operationId: '01960000-0000-7000-8000-000000000001', receiptId: '01960000-0000-7000-8000-000000000002', trackId: null } as object | null };
      const state = { connected: true, sequence: 1, workers: [], tracks: [], utterances: [word] };
      const fixture = { state, calls: [] as { opId: string; utteranceId: string; revision: number }[], stale: false, reads: 0, initial: structuredClone(word) };
      Object.assign(globalThis, { commandFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => { fixture.reads++; return fixture.stale ? { ...state, utterances: [fixture.initial] } : state; });
      ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', () => word);
      ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, request) => {
        fixture.calls.push(request);
        if (fixture.calls.length === 1) throw new Error('simulated receipt failure');
        word.state = 'landed'; word.revision++; word.presentation = null; state.sequence++;
        return word;
      });
    });
    const page = await app.firstWindow();
    await page.reload();
    const settings = page.getByRole('dialog', { name: 'Settings · App', exact: true });
    await expect(settings).toBeVisible();
    await expect(page.locator('.application-command-error')).toContainText('simulated receipt failure');
    await settings.getByRole('button', { name: 'Close settings', exact: true }).click();
    await page.getByRole('button', { name: 'Retry opening action', exact: true }).click();
    await expect(settings).toBeVisible();
    await expect(page.locator('.application-command-error')).toHaveCount(0);
    await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { commandFixture: { calls: object[] } }).commandFixture.calls.length)).toBe(2);
    const calls = await app.evaluate(() => (globalThis as typeof globalThis & { commandFixture: { calls: object[] } }).commandFixture.calls);
    expect(calls).toHaveLength(2);
    expect(calls[1]).toEqual(calls[0]);
    await settings.getByRole('button', { name: 'Close settings', exact: true }).click();
    const before = await app.evaluate(() => {
      const fixture = (globalThis as typeof globalThis & { commandFixture: { stale: boolean; reads: number } }).commandFixture;
      fixture.stale = true;
      return fixture.reads;
    });
    await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { commandFixture: { reads: number } }).commandFixture.reads)).toBeGreaterThanOrEqual(before + 2);
    await expect(settings).not.toBeVisible();
    await app.evaluate(() => { (globalThis as typeof globalThis & { commandFixture: { stale: boolean } }).commandFixture.stale = false; });
    await page.reload();
    await expect(settings).not.toBeVisible();
  } finally {
    await closeWindow(app);
    await rm(data, { recursive: true, force: true });
  }
});
