import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
let app: ElectronApplication;
let directory: string;
test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/ic-rephrase-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }) => {
    const word = { id: 'dummy-long', text: 'Original dummy specification. '.repeat(60), state: 'asked', revision: 1, target: { mode: 'new' }, attachments: [], createdAt: 1, decision: null,
      reason: 'Too long for the Editor · give it a one-line request', rephrase: { state: 'pending', proposal: null as string | null } };
    const fixture = { word, calls: [] as unknown[], fail: false };
    Object.assign(globalThis, { rephraseFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: word.revision, tracks: [], workers: [], utterances: [word] }));
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:rephrase'); ipcMain.handle('intercat:rephrase', (_event, request) => {
      fixture.calls.push(request); if (fixture.fail) { fixture.fail = false; throw new Error('Dummy lost acknowledgement'); }
      Object.assign(word, { revision: 2, state: 'staged', rephrase: null, text: request.text }); return word;
    });
  });
  await (await app.firstWindow()).reload();
});
test.afterEach(async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); });
async function proposal(text: string) {
  await app.evaluate((_, text) => {
    const fixture = (globalThis as typeof globalThis & { rephraseFixture: { word: { rephrase: { state: string; proposal: string | null } } } }).rephraseFixture;
    fixture.word.rephrase = { state: 'complete', proposal: text };
  }, text);
}
async function calls() { return app.evaluate(() => (globalThis as typeof globalThis & { rephraseFixture: { calls: unknown[] } }).rephraseFixture.calls); }

test('an arriving proposal is editable and never sends itself', async () => {
  const page = await app.firstWindow();
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await expect(field).toHaveValue('');
  await proposal('Explain the attached dummy request without changing files.');
  await expect(field).toHaveValue('Explain the attached dummy request without changing files.');
  expect(await calls()).toEqual([]);
  await field.fill('Explain the attached request in Japanese.');
  await field.press('Enter');
  await expect(page.getByRole('form', { name: 'Shorten request' })).toHaveCount(0);
  expect(await calls()).toEqual([expect.objectContaining({ utteranceId: 'dummy-long', revision: 1, text: 'Explain the attached request in Japanese.' })]);
});

test('typing wins over late suggestions and survives a reload with the same send retry', async () => {
  const page = await app.firstWindow();
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await field.fill('Keep my own dummy instruction.');
  await proposal('A different generated suggestion.');
  await expect(page.getByRole('button', { name: 'Use Editor’s draft' })).toBeVisible();
  await expect(field).toHaveValue('Keep my own dummy instruction.');
  await page.reload();
  await expect(field).toHaveValue('Keep my own dummy instruction.');
  await app.evaluate(() => { (globalThis as typeof globalThis & { rephraseFixture: { fail: boolean } }).rephraseFixture.fail = true; });
  await field.press('Enter');
  await expect(page.getByRole('button', { name: 'Retry send' })).toBeVisible();
  await expect(field).toBeDisabled();
  await page.getByRole('button', { name: 'Retry send' }).click();
  await expect(page.getByRole('form', { name: 'Shorten request' })).toHaveCount(0);
  const requests = await calls(); expect(requests).toHaveLength(2); expect(requests[0]).toEqual(requests[1]);
});

test('an arriving suggestion does not replace an in-progress composition', async () => {
  const page = await app.firstWindow();
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await expect(field).toBeEnabled();
  await field.dispatchEvent('compositionstart', { data: '' });
  await proposal('A late generated instruction.');
  await expect(page.getByRole('button', { name: 'Use Editor’s draft' })).toBeVisible();
  await expect(field).toHaveValue('');
  await field.dispatchEvent('compositionend', { data: '説明してください' });
  await field.fill('説明してください');
  expect(await calls()).toEqual([]);
});

test('draft storage failure preserves the instruction and prevents an unsaved send', async () => {
  const page = await app.firstWindow();
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await app.evaluate(() => { (globalThis as typeof globalThis & { draftStorageFixture: { failWrites: boolean } }).draftStorageFixture.failWrites = true; });
  await field.fill('Keep this instruction until its draft is saved.');
  await field.press('Enter');
  await expect(page.getByRole('button', { name: 'Retry send' })).toBeVisible();
  await expect(field).toHaveValue('Keep this instruction until its draft is saved.');
  expect(await calls()).toEqual([]);
  await app.evaluate(() => { (globalThis as typeof globalThis & { draftStorageFixture: { failWrites: boolean } }).draftStorageFixture.failWrites = false; });
  await page.getByRole('button', { name: 'Retry send' }).click();
  await expect.poll(async () => (await calls()).length).toBe(1);
});

for (const zoom of [1, 2]) test(`manual Japanese instructions remain usable at ${zoom * 100}%`, async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await field.fill('添付した仕様を説明し、ファイルは変更しないでください。');
  await field.focus();
  await expect(field).toBeInViewport();
  await field.evaluate(element => element.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true, isComposing: true })));
  expect(await calls()).toEqual([]);
  await proposal('添付した仕様の説明だけを行います。');
  await expect(field).toHaveValue('添付した仕様を説明し、ファイルは変更しないでください。');
  await page.getByRole('button', { name: 'Use Editor’s draft' }).click();
  await expect(field).toHaveValue('添付した仕様の説明だけを行います。');
  await field.focus(); await expect(field).toBeInViewport();
  await mkdir('research/verify/m2-rephrases-20261107', { recursive: true });
  const fieldPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(`research/verify/m2-rephrases-20261107/card-${zoom * 100}-field.png`, Buffer.from(fieldPixels, 'base64'));
  const send = page.getByRole('button', { name: 'Send instruction', exact: true });
  await send.focus(); await expect(send).toBeInViewport();
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(`research/verify/m2-rephrases-20261107/card-${zoom * 100}.png`, Buffer.from(pixels, 'base64'));
});

test('the card opens complete pasted originals without losing its edited instruction', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const fixture = (globalThis as typeof globalThis & { rephraseFixture: { word: Record<string, unknown> } }).rephraseFixture;
    fixture.word.sourceText = 'Original typed introduction';
    fixture.word.sources = [{ attachmentId: 'dummy-source', role: 'request', after: 'Original typed conclusion' }];
    ipcMain.removeHandler('intercat:paste-read'); ipcMain.handle('intercat:paste-read', (_event, id) => {
      if (id !== 'dummy-source') throw new Error('Wrong dummy source');
      return 'Dummy original paragraph. '.repeat(120) + 'FULL_ORIGINAL_END';
    });
  });
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await field.fill('My own short instruction.');
  await page.getByText('Original request', { exact: true }).click();
  await expect(page.getByText('Original typed introduction', { exact: true })).toBeVisible();
  await page.getByText('Read original pasted text 1', { exact: true }).click();
  await expect(page.locator('.original-paste pre')).toContainText('FULL_ORIGINAL_END');
  await expect(page.getByText('Original typed conclusion', { exact: true })).toBeVisible();
  await expect(field).toHaveValue('My own short instruction.');
  expect(await calls()).toEqual([]);
});

test('a held request keeps the users card draft when it is read again', async () => {
  const page = await app.firstWindow();
  const field = page.getByRole('textbox', { name: 'One-line request', exact: true });
  await field.fill('Keep this user-written instruction after Hold.');
  await app.evaluate(() => {
    const word = (globalThis as typeof globalThis & { rephraseFixture: { word: { revision: number; rephrase: { state: string; proposal: string | null } } } }).rephraseFixture.word;
    word.revision = 3; word.rephrase = { state: 'complete', proposal: 'A later model proposal.' };
  });
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5);
  await expect(field).toHaveValue('Keep this user-written instruction after Hold.');
  await expect(page.getByRole('button', { name: 'Use Editor’s draft' })).toBeVisible();
  await field.press('Enter');
  await expect.poll(async () => (await calls()).length).toBe(1);
  expect(await calls()).toEqual([expect.objectContaining({ revision: 3, text: 'Keep this user-written instruction after Hold.' })]);
});
