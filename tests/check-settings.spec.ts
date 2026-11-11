import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) test(`Checks keep folder choices and retry a lost save at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-check-settings-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const fixture = { setting: { revision: 0, folders: {} as Record<string, unknown> }, calls: [] as any[], lose: true };
      Object.assign(globalThis, { checkSettingsFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, tracks: [], workers: [], utterances: [], editor: { phase: 'ready', accepting: true }, privacy: { mode: 'basic', revision: 0 } }));
      ipcMain.removeHandler('intercat:checks'); ipcMain.handle('intercat:checks', () => ({ setting: fixture.setting, folders: [{ id: 'dummy-a', name: 'Dummy A' }, { id: 'dummy-b', name: 'Dummy B' }] }));
      ipcMain.removeHandler('intercat:checks-change'); ipcMain.handle('intercat:checks-change', (_event, request) => {
        fixture.calls.push(request);
        fixture.setting = { revision: 1, folders: { 'dummy-a': { revision: 1, command: request.command, history: request.history } } };
        if (fixture.lose) { fixture.lose = false; throw new Error('Dummy lost save acknowledgement'); }
        return fixture.setting;
      });
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow(); await page.reload();
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    const setting = page.getByRole('region', { name: 'Checks', exact: true });
    await setting.getByLabel('Check command', { exact: true }).selectOption('run');
    await setting.getByLabel('Command', { exact: true }).fill('printf dummy-check');
    await setting.getByLabel('Checks need Git history').check();
    await expect(setting).toContainText('including removed files and secrets');
    await setting.getByRole('button', { name: 'Apply Check settings' }).click();
    await expect(setting.getByRole('button', { name: 'Retry Check settings' })).toBeVisible();
    await expect(setting.getByLabel('Folder', { exact: true })).toBeDisabled();
    await setting.getByRole('button', { name: 'Retry Check settings' }).click();
    await expect(setting.getByRole('button', { name: 'Apply Check settings' })).toBeDisabled();
    await setting.getByLabel('Folder', { exact: true }).selectOption('dummy-b');
    await expect(setting.getByLabel('Check command', { exact: true })).toHaveValue('auto');
    await expect(setting.getByLabel('Checks need Git history')).not.toBeChecked();
    await setting.getByLabel('Folder', { exact: true }).selectOption('dummy-a');
    await expect(setting.getByLabel('Command', { exact: true })).toHaveValue('printf dummy-check');
    await expect(setting.getByLabel('Checks need Git history')).toBeChecked();
    await setting.getByRole('button', { name: 'Apply Check settings' }).scrollIntoViewIfNeeded();
    await expect(setting.getByRole('button', { name: 'Apply Check settings' })).toBeInViewport();
    const calls = await app.evaluate(() => (globalThis as any).checkSettingsFixture.calls);
    expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
