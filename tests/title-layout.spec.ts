import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) test(`generated Japanese request titles keep the original and actions readable at ${zoom * 100}%`, async () => {
  const data = await mkdtemp('/private/tmp/ic-title-layout-');
  const title = '日本語の生成した題と確認操作の表示を確かめる';
  const original = '一覧画面の説明を追加してください。設定を変えたりファイルを削除したりせず、先に確認を待ってください。';
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }, values) => {
      const state = { connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: '一覧画面', status: 'waiting' }], workers: [],
        utterances: [{ id: 'japanese-title', text: values.original, state: 'asked', revision: 1, target: { mode: 'none' }, attachments: [], createdAt: Date.now(), reason: 'Choose a track for this request.',
          decision: { kind: 'ask', trackId: null, boundary: null, command: null, description: { title: values.title, line: 'The destination needs clarification.', detail: 'No work has started.', createdAt: Date.now() } } }] };
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    }, { title, original });
    const page = await app.firstWindow();
    await page.reload();
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(zoom); }, zoom);
    const heading = page.getByRole('heading', { name: title, exact: true });
    await heading.scrollIntoViewIfNeeded();
    await expect(heading).toBeInViewport({ ratio: 1 });
    const bounds = await heading.boundingBox();
    const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width);
    const titlePixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`title-${zoom * 100}.png`), Buffer.from(titlePixels, 'base64'));
    await page.getByLabel('Read the full request', { exact: true }).click();
    await expect(page.getByLabel('Full request', { exact: true })).toHaveText(original);
    const originalText = page.getByLabel('Full request', { exact: true });
    await originalText.scrollIntoViewIfNeeded();
    await expect(originalText).toBeInViewport();
    const takeBack = page.getByRole('button', { name: 'Take back', exact: true });
    await takeBack.scrollIntoViewIfNeeded();
    await expect(takeBack).toBeInViewport({ ratio: 1 });
    await expect(page.getByRole('region', { name: 'Editor explanation', exact: true })).toHaveAttribute('data-speaking', 'false');
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`title-actions-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
  } finally { await closeWindow(app); await rm(data, { recursive: true, force: true }); }
});
