import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, LiveTrack } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

for (const zoom of [1, 2]) test(`saved work results are distinct and accessible at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-track-results-');
  const app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}`] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const tracks: LiveTrack[] = ['done', 'partial', 'failed', 'unclear'].map((kind, index) => ({
        id: String.fromCharCode(65 + index), label: String.fromCharCode(65 + index), title: `結果の例 ${index + 1}`, status: 'done',
        completion: { sessionId: 'dummy-session', turnId: `dummy-turn-${index}`, result: kind === 'unclear' ? null : {
          kind: kind as 'done' | 'partial' | 'failed', itemId: `dummy-final-${index}`, revision: 1, reason: '自作の検証用報告です。',
        } },
      }));
      const state: DaemonStatus = { connected: true, sequence: 1, workerConfigured: true,
        session: { id: 'dummy-results', name: 'Dummy result labels', slug: 'results', createdAt: 1 },
        tracks, workers: [], utterances: [],
      };
      Object.assign(globalThis, { resultFixture: state });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    });
    const page = await app.firstWindow();
    await page.reload();
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
    const grid = page.getByRole('grid', { name: 'Tracks', exact: true });
    await grid.focus();
    await grid.press('Home');
    for (const [index, label, mark, color] of [
      [0, 'done', '✓', 'rgb(111, 173, 156)'],
      [1, 'done · partial', '◐', 'rgb(208, 161, 93)'],
      [2, 'failed', '✗', 'rgb(215, 131, 131)'],
      [3, 'done · result unclear', '−', 'rgb(140, 149, 168)'],
    ] as const) {
      const letter = String.fromCharCode(65 + index);
      const row = page.getByRole('gridcell', { name: `${letter} · 結果の例 ${index + 1} · ${label}`, exact: true });
      await expect(row).toBeVisible();
      await expect(row.locator('.track-status')).toHaveText(mark);
      await expect(row.locator('.track-name small').last()).toHaveText(label);
      expect(await row.locator('.track-status').evaluate(element => getComputedStyle(element).color)).toBe(color);
      await row.scrollIntoViewIfNeeded();
      await expect(row.locator('.track-name small').last()).toBeInViewport({ ratio: 1 });
      expect(await row.locator('.track-result').evaluate(element => {
        const card = element.closest('.track-row')!.getBoundingClientRect();
        const text = element.getBoundingClientRect();
        return element.scrollWidth <= element.clientWidth + 1 && text.bottom <= card.bottom + 1;
      })).toBe(true);
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`result-${index}-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
      if (index < 3) await grid.press('ArrowDown');
    }
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { resultFixture: DaemonStatus }).resultFixture;
      state.tracks![0].status = 'working';
      state.workers = [{ trackId: 'A', phase: 'working', sessionId: 'dummy-next', activeTurn: 'dummy-new-turn',
        identity: null, detail: null, checkpointPending: false, checkpoint: null, approvals: [], prompts: [], output: null }];
      state.sequence!++;
    });
    await grid.press('Home');
    const working = page.getByRole('gridcell', { name: 'A · 結果の例 1 · working', exact: true });
    await expect(working).toBeVisible();
    await expect(working.locator('.result-done')).toHaveCount(0);
    await page.getByRole('button', { name: 'Overview', exact: true }).click();
    await expect(page.getByRole('gridcell', { name: 'C · 結果の例 3 · failed', exact: true })).toHaveAccessibleName('C · 結果の例 3 · failed');
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
