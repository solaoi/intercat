import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { CheckEnvironmentChange, CheckEnvironmentState } from '../src/bridge';

for (const zoom of [1, 2]) test(`Check environment selects a suggestion and retries its saved operation at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-check-environment-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const state: CheckEnvironmentState = { selection: { revision: 0, file: null }, candidates: ['/fixture/.env.test', '/fixture/.env.ci'], available: true };
      const calls: CheckEnvironmentChange[] = [];
      const seen = new Set<string>();
      Object.assign(globalThis, { environmentFixture: { state, calls } });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, tracks: [], workers: [], utterances: [] }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:check-environment');
      ipcMain.handle('intercat:check-environment', () => state);
      ipcMain.removeHandler('intercat:check-environment-change');
      ipcMain.handle('intercat:check-environment-change', (_event, request: CheckEnvironmentChange) => {
        calls.push(request);
        if (!seen.has(request.opId)) {
          if (request.revision !== state.selection.revision) throw new Error('The settings changed elsewhere.');
          seen.add(request.opId);
          state.selection = { revision: state.selection.revision + 1, file: request.file };
          if (calls.length === 1) throw new Error('Saved response was lost.');
        }
        return state.selection;
      });
      ipcMain.removeHandler('intercat:check-environment-choose');
      ipcMain.handle('intercat:check-environment-choose', () => '/fixture/chosen.env');
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    }, zoom);
    const page = await app.firstWindow();
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const section = page.getByRole('region', { name: 'Check environment', exact: true });
    await expect(section.getByText('None', { exact: true })).toBeVisible();
    await section.getByRole('button', { name: 'Use .env.test', exact: true }).click();
    await expect(section.getByRole('alert')).toContainText('Saved response was lost');
    await section.getByRole('button', { name: 'Retry environment save', exact: true }).click();
    await expect(section.getByText('/fixture/.env.test', { exact: true })).toBeVisible();
    const calls = await app.evaluate(() => (globalThis as typeof globalThis & { environmentFixture: { calls: CheckEnvironmentChange[] } }).environmentFixture.calls);
    expect(calls[0]).toEqual(calls[1]);
    await section.getByRole('button', { name: 'Choose file…', exact: true }).click();
    await expect(section.getByText('/fixture/chosen.env', { exact: true })).toBeVisible();
    await section.getByRole('button', { name: 'Use no file', exact: true }).click();
    await expect(section.getByText('None', { exact: true })).toBeVisible();
    await section.getByRole('button', { name: 'Use .env.ci', exact: true }).click();
    await section.scrollIntoViewIfNeeded();
    await expect(section.getByText('/fixture/.env.ci', { exact: true })).toBeInViewport({ ratio: 1 });
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('settings.png'), Buffer.from(pixels, 'base64'));
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(1));
    await page.getByRole('button', { name: 'Close settings', exact: true }).click();
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await expect(section.getByText('/fixture/.env.ci', { exact: true })).toBeVisible();
  } finally {
    await closeWindow(app);
    await rm(directory, { recursive: true, force: true });
  }
});
