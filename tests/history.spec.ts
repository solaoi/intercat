import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-history-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }) => {
    const word = { id: 'word', text: 'The taken-back request', attachments: [], target: { mode: 'none' }, state: 'cut', revision: 2, createdAt: Date.now(), decision: null, reason: null, editHold: null as null | { owner: string; token: string; revision: number } };
    const edit = { entryId: 'edit', label: 'Take back', text: word.text, at: Date.now(), blocked: null as null | string };
    const state = { connected: true, sequence: 1, session: { id: 'session', name: 'History session', slug: 'history', createdAt: 1 }, tracks: [{ id: 'A', label: 'A', title: 'Work A', status: 'waiting' }], workers: [], utterances: [word], editHistory: { undo: edit as typeof edit | null, redo: null as typeof edit | null, takenBack: [{ id: word.id, text: word.text, revision: word.revision }] } };
    const fixture = { state, calls: [] as unknown[], loseNext: false, receipts: new Map<string, typeof state>() };
    Object.assign(globalThis, { historyFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:history'); ipcMain.handle('intercat:history', (_event, request) => {
      fixture.calls.push(request);
      if (fixture.receipts.has(request.opId)) return fixture.receipts.get(request.opId);
      if (state.editHistory.undo?.blocked) throw new Error(state.editHistory.undo.blocked);
      const kind = request.action.kind;
      word.revision++;
      word.state = kind === 'redo' ? edit.label === 'Hold' ? 'held' : 'cut' : kind === 'undo' ? edit.label === 'Hold' ? 'asked' : 'placed' : 'held';
      state.editHistory.undo = kind === 'undo' ? null : edit;
      state.editHistory.redo = kind === 'undo' ? edit : null;
      state.editHistory.takenBack = kind === 'redo' && edit.label === 'Take back' ? [{ id: word.id, text: word.text, revision: word.revision }] : [];
      state.sequence++;
      fixture.receipts.set(request.opId, structuredClone(state));
      if (fixture.loseNext) { fixture.loseNext = false; throw new Error('lost history acknowledgement'); }
      return state;
    });
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', () => {
      word.state = 'held'; word.revision++; state.sequence++;
      edit.label = 'Hold'; state.editHistory.undo = edit; state.editHistory.redo = null;
      return word;
    });
    ipcMain.removeHandler('intercat:edit-request'); ipcMain.handle('intercat:edit-request', (_event, request) => {
      if (request.kind === 'begin') { word.revision++; word.editHold = { owner: 'window', token: request.token, revision: word.revision }; }
      if (request.kind === 'end') word.editHold = null;
      state.sequence++; return word;
    });
  });
  await expect((await app.firstWindow()).getByRole('heading', { name: 'History session', exact: true })).toBeVisible();
});
test.afterEach(async () => { if (app) await closeWindow(app); await rm(data, { recursive: true, force: true }); });
async function historyCalls() {
  return app.evaluate(() => (globalThis as typeof globalThis & { historyFixture: { calls: unknown[] } }).historyFixture.calls);
}
async function menu(direction: 'undo' | 'redo') {
  await app.evaluate(({ BrowserWindow, Menu }, direction) => {
    const window = BrowserWindow.getAllWindows()[0]; window.focus();
    const item = Menu.getApplicationMenu()!.getMenuItemById('timeline-' + direction)!;
    item.click(item, window, { shiftKey: false, controlKey: false, altKey: false, metaKey: false, triggeredByAccelerator: true });
  }, direction);
}

test('Undo remains available after its five-second notice and Redo uses the native menu', async () => {
  const page = await app.firstWindow(); await page.clock.install();
  // Install the clock before the component creates its notice timer.
  await page.reload();
  await expect(page.locator('.editor-line')).toContainText('Took back');
  await page.clock.runFor(5100);
  await expect(page.locator('.editor-line')).toHaveAttribute('data-speaking', 'false');
  await page.clock.resume();
  await page.getByRole('heading', { name: 'History session', exact: true }).click();
  await menu('undo');
  await expect(page.locator('.request-select')).toContainText('The taken-back request');
  await menu('redo');
  await expect(page.locator('.request-select')).toHaveCount(0);
  expect(await historyCalls()).toHaveLength(2);
});

test('the native Undo and Redo operate on composer text without touching timeline history', async () => {
  const page = await app.firstWindow();
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.focus(); await page.keyboard.type('Typed in the composer');
  await menu('undo');
  await expect(draft).not.toHaveValue('Typed in the composer');
  await menu('redo');
  await expect(draft).toHaveValue('Typed in the composer');
  await page.keyboard.press('Meta+z');
  await expect(draft).not.toHaveValue('Typed in the composer');
  await page.keyboard.press('Meta+Shift+z');
  await expect(draft).toHaveValue('Typed in the composer');
  expect(await historyCalls()).toHaveLength(0);
});

test('a lost history acknowledgement retries exactly the saved action', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { (globalThis as typeof globalThis & { historyFixture: { loseNext: boolean } }).historyFixture.loseNext = true; });
  await page.getByRole('button', { name: 'Undo', exact: true }).click();
  await page.getByRole('button', { name: 'Retry history change', exact: true }).click();
  await expect(page.locator('.history-error')).toHaveCount(0);
  const calls = await historyCalls(); expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
});

test('Taken back restores a held request or an edit draft without replacing the current draft', async () => {
  const page = await app.firstWindow();
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('Keep this ordinary draft');
  await page.getByText('Taken back · 1', { exact: true }).click();
  await page.getByRole('button', { name: 'Restore to draft', exact: true }).click();
  await expect(draft).toHaveValue('The taken-back request');
  await expect(page.getByLabel('Request being edited')).toBeVisible();
  await page.getByRole('button', { name: 'Cancel edit', exact: true }).click();
  await expect(draft).toHaveValue('Keep this ordinary draft');
  expect(await historyCalls()).toEqual([expect.objectContaining({ action: { kind: 'restoreDraft', utteranceId: 'word', revision: 2 } })]);
});

test('the running-work boundary explains why Undo is blocked', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { const state = (globalThis as typeof globalThis & { historyFixture: { state: { sequence: number; editHistory: { undo: { blocked: string | null } } } } }).historyFixture.state; state.editHistory.undo.blocked = 'can’t undo: run tests has started'; state.sequence++; });
  await expect(page.getByRole('button', { name: 'Undo', exact: true })).toHaveAttribute('title', 'can’t undo: run tests has started');
  await page.getByRole('button', { name: 'Undo', exact: true }).click();
  await expect(page.locator('.history-error')).toContainText('run tests has started');
  expect(await historyCalls()).toHaveLength(0);
});

test('Command-Z and Shift-Command-Z use timeline history outside text controls', async () => {
  const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Undo', exact: true }).focus();
  await page.keyboard.press('Meta+z');
  await expect(page.locator('.request-select')).toBeVisible();
  await page.keyboard.press('Meta+Shift+z');
  await expect(page.locator('.request-select')).toHaveCount(0);
  expect(await historyCalls()).toHaveLength(2);
});

test('the Taken back list fits at 200 percent and can restore held work', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2); });
  await page.getByText('Taken back · 1', { exact: true }).click();
  const restore = page.getByRole('button', { name: 'Restore held', exact: true });
  await restore.focus(); await expect(restore).toBeInViewport();
  await mkdir('research/verify/m2-history-20261105', { recursive: true });
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/m2-history-20261105/history-200.png', Buffer.from(pixels, 'base64'));
  await restore.click(); await expect(page.locator('.request-select')).toContainText('The taken-back request');
});

test('a request Hold joins the shared Undo and Redo history', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { historyFixture: { state: { sequence: number; utterances: { state: string; reason: string | null }[]; editHistory: { undo: unknown; takenBack: unknown[] } } } }).historyFixture.state;
    state.utterances[0].state = 'asked'; state.utterances[0].reason = 'Choose a destination';
    state.editHistory.undo = null; state.editHistory.takenBack = []; state.sequence++;
  });
  await page.getByRole('button', { name: 'Hold', exact: true }).click();
  await expect(page.locator('.request-chip.held')).toHaveCount(1);
  await page.getByRole('button', { name: 'Undo', exact: true }).click();
  await expect(page.getByRole('group', { name: 'Request needs attention' })).toBeVisible();
  await page.getByRole('button', { name: 'Redo', exact: true }).click();
  await expect(page.locator('.request-chip.held')).toHaveCount(1);
});
