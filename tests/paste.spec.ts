import { _electron as electron, expect, test, type ElectronApplication, type Locator } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts, mockPastes } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-pastes-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  await mockPastes(app);
  await app.evaluate(({ ipcMain }) => {
    const state = { connected: true, sequence: 1, session: { id: 'paste-session', name: 'Pastes', slug: 'pastes', createdAt: 1 }, tracks: [], workers: [], utterances: [] };
    Object.assign(globalThis, { pasteCalls: [], pasteState: state });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => {
      (globalThis as typeof globalThis & { pasteCalls: unknown[] }).pasteCalls.push(request);
      return { id: request.opId, text: request.text || 'Pasted request', sourceText: request.text, sources: request.sources, attachments: [], target: request.target, revision: 1, createdAt: Date.now(), state: 'staged', decision: null, reason: null };
    });
  });
  await expect((await app.firstWindow()).getByRole('heading', { name: 'Pastes', exact: true })).toBeVisible();
});
test.afterEach(async ({}, info) => {
  if (info.status === info.expectedStatus) await closeWindow(app); else app.process().kill();
  await rm(data, { recursive: true, force: true });
});
async function paste(field: Locator, text: string, start?: number, end?: number) {
  return field.evaluate((element, value) => {
    const area = element as HTMLTextAreaElement;
    area.setSelectionRange(value.start ?? area.value.length, value.end ?? value.start ?? area.value.length);
    const clipboardData = new DataTransfer(); clipboardData.setData('text/plain', value.text);
    return area.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData }));
  }, { text, start, end });
}
test('600 code points stay inline and 601 become an ordered reference beside typed text', async () => {
  const page = await app.firstWindow(); const field = page.getByRole('textbox', { name: 'Request draft', exact: true });
  await field.fill('Before AFTER');
  expect(await paste(field, '😀'.repeat(600))).toBe(true);
  expect(await paste(field, '😀'.repeat(601), 7, 7)).toBe(false);
  await expect(field).toHaveValue('Before ');
  await expect(page.getByRole('textbox', { name: 'Text after pasted block 1' })).toHaveValue('AFTER');
  await expect(page.getByRole('button', { name: 'Attach as reference', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByText('Read original pasted text', { exact: true })).toBeVisible();
  await page.getByText('Read original pasted text', { exact: true }).click();
  await expect(page.locator('.pasted-source pre')).toHaveText('😀'.repeat(601));
  await page.getByRole('button', { name: /^Put/ }).click();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { pasteCalls: { text: string; sources: unknown[] }[] }).pasteCalls);
  expect(calls).toHaveLength(1);
  expect(calls[0]).toMatchObject({ text: 'Before ', sources: [{ role: 'reference', after: 'AFTER' }] });
  expect(JSON.stringify(calls)).not.toContain('😀');
});
test('paste-only request survives reload, changes role, and preserves following text on removal', async () => {
  const page = await app.firstWindow(); const field = page.getByRole('textbox', { name: 'Request draft', exact: true });
  await expect(field).toBeEnabled();
  await paste(field, '日本語の指示。'.repeat(101));
  await expect(page.getByRole('button', { name: 'Keep as the request', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('button', { name: /^Put/ })).toBeEnabled();
  await page.getByRole('textbox', { name: 'Text after pasted block 1' }).fill('最後まで変更しないで');
  await expect.poll(async () => JSON.stringify((await page.evaluate(() => window.intercat.readDrafts('paste-session'))).book)).toContain('最後まで変更しないで');
  await page.reload();
  await expect(page.getByRole('textbox', { name: 'Text after pasted block 1' })).toHaveValue('最後まで変更しないで');
  await page.getByRole('button', { name: 'Attach as reference', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Attach as reference', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await page.getByRole('button', { name: 'Remove pasted text 1', exact: true }).click();
  await expect(field).toHaveValue('最後まで変更しないで');
  await expect(page.locator('.pasted-source')).toHaveCount(0);
});
test('failed import retries the same original after reload', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { (globalThis as typeof globalThis & { pasteFixture: { fail: boolean } }).pasteFixture.fail = true; });
  await paste(page.getByRole('textbox', { name: 'Request draft', exact: true }), 'Long instruction '.repeat(60));
  await expect(page.getByRole('button', { name: 'Retry pasted text', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: /^Put/ })).toBeDisabled();
  await page.reload();
  await expect(page.getByText('Read original pasted text', { exact: true })).toBeVisible();
  const ids = await app.evaluate(() => (globalThis as typeof globalThis & { pasteFixture: { finishes: string[] } }).pasteFixture.finishes);
  expect(ids.length).toBeGreaterThanOrEqual(2); expect(new Set(ids).size).toBe(1);
  await expect(page.getByRole('button', { name: /^Put/ })).toBeEnabled();
});
test('screening retry preserves its operation and does not submit the draft', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const requests: unknown[] = [];
    Object.assign(globalThis, { screeningRequests: requests });
    ipcMain.removeHandler('intercat:paste-refresh');
    ipcMain.handle('intercat:paste-refresh', (_event, request) => {
      requests.push(request);
      if (requests.length === 1) throw new Error('Saved reply was lost');
      return { attachmentId: request.attachmentId, sha256: 'b'.repeat(64), byteLen: 20, changed: true };
    });
  });
  const field = page.getByRole('textbox', { name: 'Request draft', exact: true });
  await field.fill('Keep this draft.');
  await paste(field, 'Reference material. '.repeat(60));
  const button = page.getByRole('button', { name: 'Recheck pasted text 1', exact: true });
  await button.click();
  await expect(page.getByText('Could not save the screening check. Retry keeps the same operation.')).toBeVisible();
  await button.click();
  await expect(page.getByText('Updated the screened copy. Resume the held request when ready.')).toBeVisible();
  const calls = await app.evaluate(() => {
    const state = globalThis as typeof globalThis & { screeningRequests: unknown[]; pasteCalls: unknown[] };
    return { screeningRequests: state.screeningRequests, pasteCalls: state.pasteCalls };
  });
  expect(calls.screeningRequests).toHaveLength(2);
  expect(calls.screeningRequests[0]).toEqual(calls.screeningRequests[1]);
  expect(calls.pasteCalls).toHaveLength(0);
  await expect(field).toHaveValue('Keep this draft.');
});
test('worker references can be paged and screened without starting work', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const state = (globalThis as typeof globalThis & { pasteState: { tracks: unknown[]; workers: unknown[] } }).pasteState;
    state.tracks = [{ id: 'a', label: 'A', title: 'Reference owner', status: 'waiting' }];
    state.workers = [{ trackId: 'a', phase: 'held', sessionId: 's', activeTurn: null, approvals: [], prompts: [], detail: 'Pasted references need screening. Open Reference screening, then Start.', checkpointPending: false }];
    let failed = false;
    ipcMain.removeHandler('intercat:paste-list');
    ipcMain.handle('intercat:paste-list', (_event, request) => {
      const first = '01960000-0000-7000-8000-000000000001';
      const second = '01960000-0000-7000-8000-000000000002';
      if (request.after && !failed) { failed = true; throw new Error('One lost page response'); }
      return { items: [{ id: request.after ? second : first, name: request.after ? 'Second reference' : 'First reference', byteLen: 20, sha256: 'a'.repeat(64) }], next: request.after ? null : first };
    });
    ipcMain.removeHandler('intercat:paste-refresh');
    ipcMain.handle('intercat:paste-refresh', (_event, request) => ({ attachmentId: request.attachmentId, sha256: 'b'.repeat(64), byteLen: 20, changed: true }));
  });
  await page.reload();
  await expect(page.getByText('Pasted references need screening. Open Reference screening, then Start.', { exact: true })).toBeVisible();
  await page.getByText('Reference screening for Worker A', { exact: true }).click();
  await expect(page.getByText('First reference · 00000001')).toBeVisible();
  await page.getByRole('button', { name: 'More references', exact: true }).click();
  await expect(page.getByText('Could not load the pasted references.')).toBeVisible();
  await page.getByRole('button', { name: 'Retry loading references', exact: true }).click();
  await expect(page.getByText('Second reference · 00000002')).toBeVisible();
  await page.getByRole('button', { name: 'Recheck reference 00000002', exact: true }).click();
  await expect(page.getByText('Updated the screened copy. Resume the held request when ready.')).toBeVisible();
  await page.getByRole('button', { name: 'Previous references', exact: true }).click();
  await expect(page.getByText('First reference · 00000001')).toBeVisible();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { pasteCalls: unknown[] }).pasteCalls);
  expect(calls).toHaveLength(0);
});
test('multiple blocks preserve insertion order and remain usable at 200 percent zoom', async () => {
  const page = await app.firstWindow(); const field = page.getByRole('textbox', { name: 'Request draft', exact: true });
  await field.fill('先頭。'); await paste(field, '一つ目の参考資料。'.repeat(80));
  const after = page.getByRole('textbox', { name: 'Text after pasted block 1' });
  await expect(after).toBeEnabled(); await after.fill('中間。末尾。');
  await paste(after, '二つ目の参考資料。'.repeat(80), 3, 3);
  await expect(page.getByRole('textbox', { name: 'Text after pasted block 2' })).toHaveValue('末尾。');
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  const second = page.getByRole('region', { name: 'Pasted text 2', exact: true });
  await second.getByRole('button', { name: 'Keep as the request', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(second.getByRole('button', { name: 'Keep as the request', exact: true })).toHaveAttribute('aria-pressed', 'true');
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('paste-200.png'), Buffer.from(pixels, 'base64'));
  await page.getByRole('button', { name: /^Put/ }).focus(); await page.keyboard.press('Enter');
  await expect(field).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { pasteCalls: unknown[] }).pasteCalls);
  expect(calls[0]).toMatchObject({ text: '先頭。', sources: [{ role: 'reference', after: '中間。' }, { role: 'request', after: '末尾。' }] });
});
