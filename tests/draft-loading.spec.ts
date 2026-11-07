import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';

test('saved-draft loading uses the composer placeholder and keeps its dimensions on restore', async () => {
  const data = await mkdtemp('/private/tmp/ic-draft-loading-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  const output = process.env.INTERCAT_DRAFT_LOADING_OUTPUT ?? 'research/verify/m2-draft-loading-20261106';
  await mkdir(output, { recursive: true });
  try {
    await app.evaluate(({ ipcMain }) => {
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({
        connected: true, sequence: 1, workerConfigured: true, editor: { phase: 'ready' },
        session: { id: 'loading', name: 'Loading draft', slug: 'loading', createdAt: 1 },
        tracks: [], workers: [], utterances: [{ id: 'ask', text: 'Choose a destination', state: 'asked',
          target: { mode: 'none' }, revision: 1, createdAt: 1, attachments: [], decision: null, reason: 'Where should this go?' }],
      }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:drafts-read');
      ipcMain.handle('intercat:drafts-read', () => new Promise(resolve => {
        Object.assign(globalThis, { finishDraftLoad: () => resolve({ revision: 1, opId: null, parentOpId: null,
          book: { active: 'work', stack: [], entries: { work: { text: 'The restored request', target: { mode: 'none' }, attachments: [], context: { kind: 'work' }, pending: null } } } }) });
      }));
    });
    const page = await app.firstWindow();
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      await page.reload();
      const input = page.getByRole('textbox', { name: 'Request draft', exact: true });
      await expect(input).toHaveAttribute('placeholder', 'Loading saved draft…');
      await expect(input).toHaveAttribute('aria-busy', 'true');
      await expect(input).toBeDisabled();
      await expect(page.getByRole('button', { name: 'Put', exact: true })).toBeDisabled();
      await expect(page.locator('.composer .draft-saving')).toHaveCount(0);
      const composer = (await page.locator('.composer').boundingBox())!;
      await expect(input).toBeInViewport();
      await page.waitForTimeout(300);
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(`${output}/loading-${zoom}.png`, Buffer.from(pixels, 'base64'));
      await app.evaluate(() => (globalThis as typeof globalThis & { finishDraftLoad: () => void }).finishDraftLoad());
      await expect(input).toHaveValue('The restored request');
      await expect(input).toBeEnabled();
      await expect(input).toHaveAttribute('aria-busy', 'false');
      const restored = (await page.locator('.composer').boundingBox())!;
      expect(restored.height).toBe(composer.height);
      expect(restored.y).toBe(composer.y);
    }
  } finally {
    await closeWindow(app);
    await rm(data, { recursive: true, force: true });
  }
});
