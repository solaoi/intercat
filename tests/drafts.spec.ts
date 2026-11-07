import { mockDrafts, mockPastes } from './support/drafts';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { closeWindow } from './support/close';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-drafts-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  await mockPastes(app);
  await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const question = (id: string, trackId: string) => ({ id, trackId, reportId: id, sessionId: 'worker-' + trackId, turnId: 'turn-' + trackId, revision: 1, kind: 'question', text: 'Tell me what to do', state: 'open', replyUtteranceId: null });
    const state = { connected: true, sequence: 1, session: { id: 'session-one', name: 'Session one', slug: 'one', createdAt: 1 },
      tracks: ['A', 'B', 'C'].map(id => ({ id, label: id, title: 'Work ' + id, status: 'waiting' })), workers: [], utterances: [] as { id: string; state: string; revision: number; target: unknown; replyTo?: unknown }[], reportQuestions: [question('q-a', 'A'), question('q-c', 'C')] };
    const history = new Map(state.reportQuestions.map(question => [question.id, structuredClone(question)]));
    const fixture = { state, calls: [] as unknown[], lose: false };
    Object.assign(globalThis, { draftFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:attach-reference'); ipcMain.handle('intercat:attach-reference', () => ({ id: 'reference', name: 'notes.txt', byteLen: 12, sha256: 'a'.repeat(64) }));
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => {
      fixture.calls.push(request);
      if (fixture.lose) { fixture.lose = false; throw new Error('lost acknowledgement'); }
      const saved = { id: request.opId, text: request.text, attachments: [], target: request.target, revision: 1, createdAt: Date.now(), state: 'staged', decision: null, reason: null };
      state.utterances.push(saved); state.sequence++; return saved;
    });
    ipcMain.removeHandler('intercat:report-response'); ipcMain.handle('intercat:report-response', (_event, request) => {
      fixture.calls.push(request);
      const question = state.reportQuestions.find(question => question.id === request.questionId);
      const original = history.get(request.questionId)!;
      const changed = !!question && question.revision !== request.revision;
      const orphaned = !question || changed;
      const saved = { id: request.opId, text: request.answer.text, target: { mode: 'lock', trackId: original.trackId }, revision: 1, createdAt: Date.now(), state: orphaned ? 'orphaned' : 'placed', decision: orphaned ? null : { kind: 'extend', trackId: original.trackId }, attachments: [], reason: orphaned ? changed ? 'the question changed meanwhile' : 'the question was answered meanwhile' : null, replyTo: { id: original.id, revision: original.revision, question: original } };
      if (question && !orphaned) question.replyUtteranceId = request.opId;
      state.utterances.push(saved); state.sequence++; return saved;
    });
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, request) => {
      fixture.calls.push({ kind: 'resolve', ...request });
      const word = state.utterances.find(word => word.id === request.utteranceId)!;
      word.state = 'placed'; word.replyTo = null; word.target = request.resolution.target; word.revision++; state.sequence++; return word;
    });
    ipcMain.removeHandler('intercat:cut'); ipcMain.handle('intercat:cut', (_event, id) => {
      const word = state.utterances.find(word => word.id === id)!;
      word.state = 'cut'; word.revision++; state.sequence++; return word;
    });
  });
  await expect((await app.firstWindow()).getByRole('heading', { name: 'Session one', exact: true })).toBeVisible();
});
test.afterEach(async ({}, info) => {
  if (app) {
    if (info.status === info.expectedStatus) await closeWindow(app);
    else app.process().kill();
  }
  await rm(data, { recursive: true, force: true });
});

for (const [label, character, extra, accepted] of [
  ['ASCII at the limit', 'a', 0, true],
  ['Japanese at the limit', '字', 0, true],
  ['ASCII above the limit', 'a', 1, false],
  ['Japanese above the limit', '字', 1, false],
] as const) {
  test(`paste admission measures UTF-8 bytes for ${label}`, async () => {
    const page = await app.firstWindow();
    const draft = page.getByRole('textbox', { name: 'Request draft' });
    await draft.fill('Keep this request');
    const result = await draft.evaluate((element, { character, extra }) => {
      const limit = 2 * 1024 * 1024;
      const size = new TextEncoder().encode(character).byteLength;
      const text = character.repeat(Math.floor(limit / size)) + 'a'.repeat(limit % size + extra);
      const clipboard = new DataTransfer();
      clipboard.setData('text/plain', text);
      const event = new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData: clipboard });
      return { accepted: element.dispatchEvent(event), bytes: new TextEncoder().encode(text).byteLength };
    }, { character, extra });
    expect(result).toEqual({ accepted: false, bytes: 2 * 1024 * 1024 + extra });
    await expect(draft).toHaveValue('Keep this request');
    if (accepted) await expect(page.getByText('Read original pasted text', { exact: true })).toBeVisible();
    else await expect(page.locator('.toast')).toHaveText('This paste exceeds 2 MiB. Shorten it and try again.');
  });
}

test('two Reply drafts return in order and preserve the ordinary locked draft and reference', async () => {
  const page = await app.firstWindow();
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('Instruction for B');
  await page.getByRole('combobox', { name: 'Request destination' }).selectOption('B');
  await page.getByRole('button', { name: 'Attach reference file', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Remove reference notes.txt' })).toBeVisible();
  const more = page.getByRole('button', { name: /^\+\d+ more/ });
  if (await more.isVisible()) await more.click();
  await page.getByRole('group', { name: 'Question from Worker A' }).getByRole('button', { name: 'Reply', exact: true }).click();
  await expect(draft).toHaveValue(''); await draft.fill('Reply for A');
  const questionC = page.getByRole('group', { name: 'Question from Worker C' });
  if (!await questionC.isVisible()) await more.click();
  await questionC.getByRole('button', { name: 'Reply', exact: true }).click();
  await expect(draft).toHaveValue(''); await draft.fill('Reply for C');
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2); });
  await expect(page.getByRole('button', { name: 'Send reply', exact: false })).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Reply to Worker A · Resume', exact: true })).toBeInViewport();
  await mkdir('research/verify/m2-drafts-20261105', { recursive: true });
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/m2-drafts-20261105/reply-200.png', Buffer.from(pixels, 'base64'));
  await page.getByRole('button', { name: 'Send reply', exact: false }).click();
  await expect(draft).toHaveValue('Reply for A');
  await page.getByRole('button', { name: 'Cancel reply', exact: true }).click();
  await expect(draft).toHaveValue('Instruction for B');
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
  await expect(page.getByRole('button', { name: 'Remove reference notes.txt' })).toBeVisible();
  await page.getByRole('button', { name: 'Reply to Worker A · Resume', exact: true }).click();
  await expect(draft).toHaveValue('Reply for A');
});

test('session drafts survive reload and remain separate when switching sessions', async () => {
  const page = await app.firstWindow();
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('First session draft');
  await page.getByRole('combobox', { name: 'Request destination' }).selectOption('B');
  await page.reload();
  await expect(draft).toHaveValue('First session draft');
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { draftFixture: { state: { session: { id: string; name: string }; sequence: number } } }).draftFixture; fixture.state.session.id = 'session-two'; fixture.state.session.name = 'Session two'; fixture.state.sequence++; });
  await expect(page.getByRole('heading', { name: 'Session two', exact: true })).toBeVisible();
  await expect(draft).toHaveValue(''); await draft.fill('Second session draft');
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { draftFixture: { state: { session: { id: string; name: string }; sequence: number } } }).draftFixture; fixture.state.session.id = 'session-one'; fixture.state.session.name = 'Session one'; fixture.state.sequence++; });
  await expect(draft).toHaveValue('First session draft');
});

test('an unconfirmed send survives reload and retries its original operation', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { (globalThis as typeof globalThis & { draftFixture: { lose: boolean } }).draftFixture.lose = true; });
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('Retry this exact request');
  await page.getByRole('button', { name: 'Put', exact: false }).click();
  await expect(page.getByRole('button', { name: 'Retry save', exact: false })).toBeVisible();
  await page.reload();
  await expect(draft).toHaveValue('Retry this exact request');
  await page.getByRole('button', { name: 'Retry save', exact: false }).click();
  await expect(draft).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { draftFixture: { calls: unknown[] } }).draftFixture.calls);
  expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
});

test('concurrent windows retain local text and require an explicit conflict choice', async () => {
  const first = await app.firstWindow();
  const opened = app.waitForEvent('window');
  await app.evaluate(async ({ BrowserWindow }, preload) => {
    const url = BrowserWindow.getAllWindows()[0].webContents.getURL();
    const window = new BrowserWindow({ width: 1040, height: 760, webPreferences: { preload, contextIsolation: true, nodeIntegration: false, sandbox: true } });
    await window.loadURL(url);
  }, path.resolve('electron/preload.cjs'));
  const second = await opened;
  const one = first.getByRole('textbox', { name: 'Request draft' });
  const two = second.getByRole('textbox', { name: 'Request draft' });
  await expect(two).toBeEnabled();
  await one.fill('Saved by the first window');
  await expect.poll(async () => (await first.evaluate(() => window.intercat.readDrafts('session-one'))).book.entries.work.text).toBe('Saved by the first window');
  await two.fill('Kept in the second window');
  await expect(second.getByRole('alert')).toContainText('another window');
  await expect(two).toHaveValue('Kept in the second window');
  await second.getByRole('button', { name: 'Keep this draft', exact: true }).click();
  await expect(second.getByRole('alert')).toHaveCount(0);
  await second.reload(); await expect(two).toHaveValue('Kept in the second window');
  await one.fill('Another local change');
  await expect(first.getByRole('alert')).toContainText('another window');
  await first.getByRole('button', { name: 'Reload saved draft', exact: true }).click();
  await expect(one).toHaveValue('Kept in the second window');
  await expect(first.getByRole('alert')).toHaveCount(0);
});

for (const changed of [false, true]) {
  test(`an obsolete Reply from a ${changed ? 'changed' : 'previously answered'} question keeps its original context`, async () => {
    const page = await app.firstWindow();
    const draft = page.getByRole('textbox', { name: 'Request draft' });
    await draft.fill('My ordinary draft');
    await page.getByRole('group', { name: 'Question from Worker A' }).getByRole('button', { name: 'Reply', exact: true }).click();
    await draft.fill('My original answer');
    await app.evaluate((_electron, changed) => {
      const fixture = (globalThis as typeof globalThis & { draftFixture: { state: { sequence: number; reportQuestions: { id: string; revision: number; text: string }[] } } }).draftFixture;
      if (changed) { fixture.state.reportQuestions[0].revision++; fixture.state.reportQuestions[0].text = 'A completely different question'; }
      else fixture.state.reportQuestions = fixture.state.reportQuestions.filter(question => question.id !== 'q-a');
      fixture.state.sequence++;
    }, changed);
    await expect(page.getByRole('button', { name: 'Keep reply for review', exact: false })).toBeVisible();
    await expect(draft).toHaveValue('My original answer');
    await page.getByRole('button', { name: 'Keep reply for review', exact: false }).click();
    await expect(draft).toHaveValue('My ordinary draft');
    const reply = page.getByRole('group', { name: 'Reply needs a destination', exact: true });
    await expect(page.locator('.orphaned-request')).toBeAttached();
    await page.getByRole('button', { name: /^\+\d+ more/ }).click();
    await expect(reply).toContainText(changed ? 'the question changed meanwhile' : 'the question was answered meanwhile');
    await reply.getByText('Original question', { exact: true }).click();
    await expect(reply).toContainText('Tell me what to do');
    await expect(reply).not.toContainText('A completely different question');
    await page.reload();
    await expect(page.locator('.orphaned-request')).toBeAttached();
    await page.getByRole('button', { name: /^\+\d+ more/ }).click();
    await expect(reply).toContainText('My original answer');
    await expect(reply.getByRole('button', { name: 'Send instruction', exact: true })).toBeDisabled();
    if (changed) {
      await reply.getByRole('combobox', { name: 'Reply destination' }).focus();
      await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2); });
      await reply.getByRole('combobox', { name: 'Reply destination' }).focus();
      await expect(reply.getByRole('combobox', { name: 'Reply destination' })).toBeInViewport();
      await mkdir('research/verify/m2-orphaned-20261105', { recursive: true });
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile('research/verify/m2-orphaned-20261105/orphaned-200.png', Buffer.from(pixels, 'base64'));
    }
    await reply.getByRole('combobox', { name: 'Reply destination' }).selectOption('B');
    await reply.getByRole('button', { name: 'Send instruction', exact: true }).click();
    await expect(reply).toHaveCount(0);
    const calls = await app.evaluate(() => (globalThis as typeof globalThis & { draftFixture: { calls: { kind?: string; resolution?: unknown }[] } }).draftFixture.calls);
    expect(calls.filter(call => call.kind === 'resolve')).toEqual([expect.objectContaining({ resolution: { action: 'retry', target: { mode: 'lock', trackId: 'B' } } })]);
  });
}

test('taking back an obsolete Reply keeps the ordinary draft', async () => {
  const page = await app.firstWindow();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Keep this draft');
  await page.getByRole('group', { name: 'Question from Worker A' }).getByRole('button', { name: 'Reply', exact: true }).click();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('An obsolete answer');
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { draftFixture: { state: { reportQuestions: unknown[]; sequence: number } } }).draftFixture; fixture.state.reportQuestions = []; fixture.state.sequence++; });
  await page.getByRole('button', { name: 'Keep reply for review', exact: false }).click();
  await expect(page.locator('.orphaned-request')).toBeAttached();
  await page.getByRole('button', { name: /^\+\d+ more/ }).click();
  await page.getByRole('group', { name: 'Reply needs a destination' }).getByRole('button', { name: 'Take back', exact: true }).click();
  await expect(page.getByRole('group', { name: 'Reply needs a destination' })).toHaveCount(0);
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Keep this draft');
});
