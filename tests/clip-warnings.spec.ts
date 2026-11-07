import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const kind of ['duplicate-title', 'same-file']) for (const zoom of [1, 2]) {
  test(`${kind} warnings refresh both clips and stay accessible at ${zoom * 100}%`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-clip-warnings-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      await mockDrafts(app);
      await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
        const window = BrowserWindow.getAllWindows()[0];
        window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
        const now = Date.now();
        const tracks = ['A', 'B'].map(id => ({ id, label: id, title: 'キャッシュの再試行', status: 'done' }));
        const clips = tracks.map((track, index) => ({ id: `clip-${track.id}`, trackId: track.id, repairRunId: null, ordinal: 1,
          sessionId: `session-${track.id}`, turnId: 'turn', category: 'step', title: 'Cache retry', label: 'Added a cache retry.',
          summary: 'Added a cache retry.', kind: 'edit', start: now - 180_000 + index * 20_000, end: now - 60_000 + index * 20_000,
          generationEnd: now - 100_000, running: false, approximate: false, toolTicks: [], warnings: [] as { kind: string; text: string }[] }));
        const state = { connected: true, sequence: 1, tracks, workers: [], utterances: [], proposals: [],
          timeline: { clock: { timelineMs: now, epochMs: now, monoMs: 1, bootId: 'fixture' }, summaries: [] } };
        Object.assign(globalThis, { clipWarningsFixture: { state, clips } });
        ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
        ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips, removed: [], marks: [], removedMarks: [] }));
        ipcMain.removeHandler('intercat:clip-detail'); ipcMain.handle('intercat:clip-detail', (_event, id: string) => ({ clip: clips.find(clip => clip.id === id), items: [] }));
      }, zoom);
      const page = await app.firstWindow();
      await page.reload();
      const timeline = page.getByRole('region', { name: 'Timeline', exact: true });
      await expect(timeline).toHaveAttribute('data-ready', 'true');
      await expect(page.getByRole('button', { name: /^Open step A:1/ })).toHaveCount(1);
      await expect(page.locator('.step-actions [data-warning]')).toHaveCount(0);
      await app.evaluate((_, kind) => {
        const fixture = (globalThis as typeof globalThis & { clipWarningsFixture: {
          state: { sequence: number }; clips: { trackId: string; warnings: { kind: string; text: string }[] }[];
        } }).clipWarningsFixture;
        for (const clip of fixture.clips) clip.warnings = kind === 'duplicate-title'
          ? [{ kind, text: `Same title as ${clip.trackId === 'A' ? 'B' : 'A'}:1 within 10 minutes; possible duplicate work.` }]
          : [{ kind, text: `Also changed in ${clip.trackId === 'A' ? 'B' : 'A'}:1 · src/キャッシュの再試行と保存する値の検証.ts. Both tracks have uncommitted changes.` },
            { kind, text: 'Also changed in C:2 · another/file.ts. Both tracks have uncommitted changes.' }];
        fixture.state.sequence++;
      }, kind);
      const buttons = page.locator('.step-actions [data-warning]');
      await expect(buttons).toHaveCount(2);
      const first = page.getByRole('button', { name: /^Open step A:1.*Warning:/ });
      const expected = kind === 'duplicate-title' ? 'Same title as B:1 within 10 minutes' : 'Also changed in B:1';
      await expect(first).toHaveAttribute('title', new RegExp(expected));
      await first.hover();
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`warning-${zoom}.png`), Buffer.from(pixels, 'base64'));
      await first.focus(); await first.press('Enter');
      const warning = page.getByRole('region', { name: 'Step warnings', exact: true });
      await expect(warning).toContainText(expected);
      await expect(warning.locator('p')).toHaveCount(kind === 'duplicate-title' ? 1 : 2);
      await warning.scrollIntoViewIfNeeded();
      await expect(warning).toBeInViewport();
      const bounds = await warning.evaluate(element => ({ content: element.scrollWidth, width: element.clientWidth }));
      expect(bounds.content).toBeLessThanOrEqual(bounds.width);
      const detail = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`detail-${zoom}.png`), Buffer.from(detail, 'base64'));
      await page.getByRole('button', { name: 'Overview', exact: true }).click();
      await expect(timeline).toHaveAttribute('data-overview', 'true');
      await expect(page.getByRole('gridcell', { name: /^A ·.*1 step warnings/ })).toHaveCount(1);
      const overview = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`overview-${zoom}.png`), Buffer.from(overview, 'base64'));
      await app.evaluate(() => {
        const fixture = (globalThis as typeof globalThis & { clipWarningsFixture: {
          state: { sequence: number }; clips: { warnings: unknown[] }[];
        } }).clipWarningsFixture;
        for (const clip of fixture.clips) clip.warnings = [];
        fixture.state.sequence++;
      });
      await expect(warning).toHaveCount(0);
      await expect(page.getByRole('gridcell', { name: /step warnings/ })).toHaveCount(0);
      await page.getByRole('button', { name: 'Detail', exact: true }).click();
      await expect(page.locator('.step-actions [data-warning]')).toHaveCount(0);
    } finally {
      await closeWindow(app);
      await rm(directory, { recursive: true, force: true });
    }
  });
}
