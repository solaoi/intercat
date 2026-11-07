import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) {
  test(`an assumption marker opens its finished clip at the extraction time at ${zoom * 100}%`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-assumption-time-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      await mockDrafts(app);
      const at = Date.now();
      await app.evaluate(({ ipcMain, BrowserWindow }, { at, zoom }) => {
        BrowserWindow.getAllWindows()[0].setSize(1040, 760);
        BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
        const assumption = { id: 'choice', source: 'default-placement', sourceId: 'message', trackId: 'A', clipId: 'clip', question: 'Which database?', options: ['SQLite', 'PostgreSQL'], text: 'SQLite', evidence: 'I assumed SQLite.', revision: 1, state: 'open', keptAt: null, changedAt: null, changeInputId: null, observedAt: { epochMs: at - 3_620_000, timelineMs: at - 20_000, monoMs: 1, bootId: 'fixture' } };
        const clip = { id: 'clip', trackId: 'A', repairRunId: null, ordinal: 1, sessionId: 'session', turnId: 'turn', category: 'step', label: 'Old work', kind: 'read', start: at - 600_000, end: at - 500_000, generationEnd: at - 500_000, running: false, approximate: false, toolTicks: [], assumptionCount: 1, assumptionTimes: [at - 20_000] };
        ipcMain.removeHandler('intercat:status');
        ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: 'Cache', status: 'done' }], workers: [], utterances: [], assumptions: [], timeline: { clock: { timelineMs: at, epochMs: at, monoMs: 1, bootId: 'fixture' }, summaries: [] } }));
        ipcMain.removeHandler('intercat:timeline-window');
        ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [clip], marks: [], removed: [], removedMarks: [] }));
        ipcMain.removeHandler('intercat:clip-detail');
        ipcMain.handle('intercat:clip-detail', () => ({ clip, assumptions: [assumption], items: [], effects: [], deletionSnapshots: [] }));
      }, { at, zoom });
      const page = await app.firstWindow();
      await page.reload();
      const timeline = page.getByRole('region', { name: 'Timeline', exact: true });
      await expect(timeline).toHaveAttribute('data-ready', 'true');
      const marker = page.getByRole('button', { name: /^Open assumptions for A:1/ });
      await expect(marker).toBeVisible();
      await expect(page.getByRole('button', { name: /^Open step A:1/ })).toHaveCount(0);
      const markerBox = (await marker.boundingBox())!;
      const timelineBox = (await timeline.boundingBox())!;
      expect(markerBox.x).toBeGreaterThan(timelineBox.x + timelineBox.width * 0.5);
      await marker.focus();
      await expect(marker).toBeFocused();
      const markerPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`marker-${zoom}.png`), Buffer.from(markerPixels, 'base64'));
      await marker.press('Enter');
      const assumptions = page.getByLabel('Step assumptions');
      await expect(assumptions).toContainText('SQLite');
      await expect(assumptions.locator('time')).toHaveAttribute('datetime', new Date(at - 3_620_000).toISOString());
      await assumptions.locator('time').evaluate(element => element.scrollIntoView({ block: 'center' }));
      await expect(assumptions.locator('time')).toBeInViewport({ ratio: 1 });
      await page.waitForTimeout(150);
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`detail-${zoom}.png`), Buffer.from(pixels, 'base64'));
    } finally {
      await closeWindow(app);
      await rm(directory, { recursive: true, force: true });
    }
  });
}
