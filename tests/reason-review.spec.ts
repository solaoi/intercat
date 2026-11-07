import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

for (const zoom of [1, 2]) test(`the sent reason-review label and details remain readable at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-reason-review-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const word = { id: 'dummy-review', text: 'まだおかしい。', state: 'sent', revision: 1, createdAt: 1,
        target: { mode: 'lock', trackId: 'A' }, attachments: [], reason: null,
        decision: { kind: 'extend', trackId: 'A', boundary: null, command: null },
        reasonReview: { label: 'A', reportId: 'dummy-report', reportRevision: 1, turnId: 'dummy-turn', checkpoint: 'a'.repeat(40) } };
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, utterances: [word], workers: [], tracks: [{ id: 'A', label: 'A', title: 'ダミーの確認', status: 'ready' }] }));
      ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', () => word);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow(); await page.reload();
    const label = page.getByText('sent with A’s reasons', { exact: true });
    await expect(label).toBeVisible();
    await label.scrollIntoViewIfNeeded();
    await expect(label).toBeInViewport();
    await mkdir('research/verify/m2-reason-review-20261107', { recursive: true });
    const chipPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(`research/verify/m2-reason-review-20261107/chip-${zoom * 100}.png`, Buffer.from(chipPixels, 'base64'));
    await page.locator('[data-utterance-id="dummy-review"]').click();
    const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(dialog.getByText('Sent with A’s reasons', { exact: true })).toBeVisible();
    await expect(dialog.getByText('まだおかしい。', { exact: true })).toBeVisible();
    const close = dialog.getByRole('button', { name: 'Close request details' });
    await close.focus(); await expect(close).toBeInViewport();
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(`research/verify/m2-reason-review-20261107/review-${zoom * 100}.png`, Buffer.from(pixels, 'base64'));
    await close.click(); await expect(dialog).toHaveCount(0);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
