import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

async function fixture(full = false) {
  const directory = await mkdtemp('/private/tmp/ic-privacy-settings-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }, full) => {
    const state = { connected: true, sequence: 1, privacy: { mode: full ? 'full' : 'basic', revision: 0 }, privacyRevision: 0,
      editor: { phase: 'stopped', modelLoaded: false, accepting: false }, tracks: [], workers: [], utterances: [] };
    const fixture = { state, calls: [] as unknown[], lose: false, saved: null as null | typeof state.privacy };
    Object.assign(globalThis, { privacySettingsFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:privacy-change'); ipcMain.handle('intercat:privacy-change', (_event, request) => {
      fixture.calls.push(request);
      fixture.saved ??= { mode: request.mode, revision: request.revision + 1 };
      if (fixture.lose) { fixture.lose = false; throw new Error('Lost privacy setting acknowledgement'); }
      state.privacy = fixture.saved; state.privacyRevision = state.privacy.revision; state.sequence++;
      return state.privacy;
    });
  }, full);
  const page = await app.firstWindow(); await page.reload();
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  return { app, page, setting: page.getByRole('region', { name: 'Privacy gate', exact: true }), cleanup: async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); } };
}

test('Basic is explicit and Full becomes available only with the local Editor', async () => {
  const { app, setting, cleanup } = await fixture();
  try {
    await expect(setting.getByRole('combobox')).toHaveValue('basic');
    await expect(setting.locator('option[value="full"]')).toBeDisabled();
    await app.evaluate(() => { const f = (globalThis as any).privacySettingsFixture; f.state.editor = { phase: 'ready', modelLoaded: true, accepting: true }; f.state.sequence++; });
    await expect(setting.locator('option[value="full"]')).toBeEnabled();
    await setting.getByRole('combobox').selectOption('full');
    await expect(setting).toContainText('Also checks names, addresses and passwords on this Mac.');
    expect(await app.evaluate(() => (globalThis as any).privacySettingsFixture.calls)).toHaveLength(1);
  } finally { await cleanup(); }
});

test('a lost acknowledgement retries the same privacy operation', async () => {
  const { app, setting, cleanup } = await fixture();
  try {
    await app.evaluate(() => { const f = (globalThis as any).privacySettingsFixture; f.lose = true; f.state.editor = { phase: 'ready', modelLoaded: true, accepting: true }; f.state.sequence++; });
    await expect(setting.locator('option[value="full"]')).toBeEnabled();
    await setting.getByRole('combobox').selectOption('full');
    await setting.getByRole('button', { name: 'Retry privacy setting' }).click();
    await expect(setting.getByRole('combobox')).toHaveValue('full');
    const calls = await app.evaluate(() => (globalThis as any).privacySettingsFixture.calls);
    expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
  } finally { await cleanup(); }
});

test('an unavailable Full mode stays explicit and can be switched to Basic at 200 percent', async () => {
  const { app, page, setting, cleanup } = await fixture(true);
  try {
    await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
    await expect(setting).toContainText('Full requests wait until the local Editor is ready.');
    await setting.getByRole('combobox').selectOption('basic');
    await expect(setting.getByRole('combobox')).toHaveValue('basic');
    await setting.getByRole('combobox').scrollIntoViewIfNeeded();
    await expect(setting.getByRole('combobox')).toBeInViewport();
    await expect(page.getByRole('dialog', { name: 'Settings · App' })).toBeVisible();
  } finally { await cleanup(); }
});
