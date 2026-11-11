import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) test(`checkpoint exclusion keeps Stop available and retries the same decision at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-checkpoint-card-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }) => {
      const issue = { id: '00000000-0000-5000-8000-000000000012', trackId: 'A', sessionId: 'dummy-session', turnId: '00000000-0000-7000-8000-000000000013', revision: 3, path: 'vendor/長い名前のダミー資料/別のリポジトリ' };
      const worker = { trackId: 'A', phase: 'checkpoint-blocked', sessionId: 'dummy-session', identity: null, activeTurn: null, approvals: [], prompts: [], checkpointPending: true, checkpoint: null, output: null, detail: null };
      const state = { issues: [issue], calls: [] as { opId: string; issueId: string; revision: number }[], stops: 0, release: null as (() => void) | null };
      Object.assign(globalThis, { checkpointFixture: state });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: state.calls.length + state.stops + state.issues.length + 1, dispatchPaused: true, tracks: [{ id: 'A', label: 'A', title: 'Generated checkpoint fixture', status: 'working' }], workers: [worker], checkpointIssues: state.issues }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:checkpoint-exclude');
      ipcMain.handle('intercat:checkpoint-exclude', async (_event, request) => {
        state.calls.push(request);
        if (state.calls.length === 1) throw new Error('Dummy lost acknowledgement');
        await new Promise<void>(resolve => { state.release = resolve; });
        state.issues.splice(0);
        worker.phase = 'held'; worker.checkpointPending = false;
      });
      ipcMain.removeHandler('intercat:worker-stop');
      ipcMain.handle('intercat:worker-stop', () => { state.stops++; worker.phase = 'held'; });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    });
    const page = await app.firstWindow();
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const card = page.locator('[data-attention-card="checkpoint-issue-00000000-0000-5000-8000-000000000012"]');
    await expect(card).toBeVisible();
    await expect(card).toContainText('vendor/長い名前のダミー資料/別のリポジトリ is a separate repository');
    await card.hover();
    const explanation = card.locator('strong');
    await explanation.scrollIntoViewIfNeeded();
    expect(await explanation.evaluate(element => { const r = element.getBoundingClientRect(); return element.contains(document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)); })).toBe(true);
    const initial = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('checkpoint-explanation.png'), Buffer.from(initial, 'base64'));
    const scope = card.locator('p').first();
    await scope.scrollIntoViewIfNeeded();
    expect(await scope.evaluate(element => { const r = element.getBoundingClientRect(); return element.contains(document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)); })).toBe(true);
    const exclude = card.getByRole('button', { name: 'Exclude it', exact: true });
    const stop = card.getByRole('button', { name: 'Stop', exact: true });
    await expect(exclude).toHaveAccessibleDescription(/^vendor\/長い名前のダミー資料\/別のリポジトリ/);
    await expect(stop).toHaveAccessibleDescription(/^vendor\/長い名前のダミー資料\/別のリポジトリ/);
    await exclude.click();
    await expect(card.getByRole('alert')).toHaveText('Dummy lost acknowledgement');
    await stop.click();
    await expect(exclude).toBeEnabled();
    await exclude.click();
    await expect(exclude).toBeDisabled();
    await expect(stop).toBeEnabled();
    await stop.focus();
    const hit = await stop.evaluate(element => { const r = element.getBoundingClientRect(); return element.contains(document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)); });
    expect(hit).toBe(true);
    const spacing = await card.locator('[data-card-actions]').evaluate(element => { const [first, second] = [...element.querySelectorAll('button')].map(button => button.getBoundingClientRect()); return second.left - first.right; });
    expect(spacing).toBeGreaterThanOrEqual(8);
    const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('checkpoint-card.png'), Buffer.from(capture, 'base64'));
    await stop.press('Enter');
    const fixture = await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { checkpointFixture: { calls: { opId: string; issueId: string; revision: number }[]; stops: number; release: (() => void) | null } }).checkpointFixture;
      state.release?.(); return { calls: state.calls, stops: state.stops };
    });
    expect(fixture.calls).toHaveLength(2);
    expect(fixture.calls[0]).toEqual(fixture.calls[1]);
    expect(fixture.calls[0].revision).toBe(3);
    expect(fixture.stops).toBe(2);
    await page.mouse.move(0, 0);
    await expect(card).toHaveCount(0);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
