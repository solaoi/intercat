import { mockDrafts } from './support/drafts';
import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { closeWindow } from './support/close';
import type { DaemonStatus, WorkerState } from '../src/bridge';

let app: ElectronApplication;
let directory: string;

test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/intercat-file-drop-');
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}/app`] });
  await mockDrafts(app);
  await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const state = { connected: true, sequence: 1, workerConfigured: true, workers: [], utterances: [] as unknown[],
      tracks: [{ id: 'A', label: 'A', title: 'First task', status: 'waiting' }, { id: 'B', label: 'B', title: 'Second task', status: 'waiting' }] };
    const fixture = { state, imports: [] as string[], requests: [] as unknown[], failFirst: false, holdImport: false, release: () => {} };
    Object.assign(globalThis, { dropFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:attach-dropped-reference');
    ipcMain.handle('intercat:attach-dropped-reference', async (_event, source: string) => {
      fixture.imports.push(source);
      if (fixture.failFirst && fixture.imports.length === 1) throw new Error('lost acknowledgement');
      if (fixture.holdImport) await new Promise<void>(resolve => { fixture.release = resolve; });
      const name = source.split('/').at(-1)!;
      return { id: name.includes('first') ? 'first-reference' : 'second-reference', name, byteLen: 8, sha256: 'a'.repeat(64) };
    });
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => {
      fixture.requests.push(request);
      const word = { id: request.opId, text: request.text, attachments: [], state: 'staged', target: request.target, createdAt: Date.now(), revision: 1, decision: null, reason: null };
      state.utterances = [word]; state.sequence++;
      return word;
    });
  });
});

test.afterEach(async () => {
  if (app) await closeWindow(app);
  await rm(directory, { recursive: true, force: true });
});

async function selectFiles(page: Page, names: string[]) {
  for (const name of names) await writeFile(path.join(directory, name), 'reference');
  await page.evaluate(() => { const input = document.createElement('input'); input.type = 'file'; input.multiple = true; input.id = 'drop-source'; input.hidden = true; document.body.append(input); });
  await page.locator('#drop-source').setInputFiles(names.map(name => path.join(directory, name)));
}

async function dispatchDrop(page: Page, selector: string, type: 'dragover' | 'drop') {
  await page.locator(selector).scrollIntoViewIfNeeded();
  const bounds = await page.locator(selector).boundingBox();
  if (!bounds) throw new Error('Drop destination is not visible');
  await page.evaluate(({ selector, type, x, y }) => {
    const transfer = new DataTransfer();
    for (const file of (document.querySelector('#drop-source') as HTMLInputElement).files!) transfer.items.add(file);
    document.querySelector(selector)!.dispatchEvent(new DragEvent(type, { bubbles: true, cancelable: true, dataTransfer: transfer, clientX: x, clientY: y }));
  }, { selector, type, x: bounds.x + bounds.width / 2, y: bounds.y + bounds.height / 2 });
}

test('lane drop locks the destination and retries every remaining file without sending a request', async () => {
  const page = await app.firstWindow();
  await page.locator('#track-A').click();
  await app.evaluate(() => { (globalThis as typeof globalThis & { dropFixture: { failFirst: boolean } }).dropFixture.failFirst = true; });
  await selectFiles(page, ['first.txt', 'second.txt']);
  await dispatchDrop(page, '#track-B', 'dragover');
  await expect(page.getByText('Drop to attach · to Worker B', { exact: true })).toBeVisible();
  await dispatchDrop(page, '#track-B', 'drop');
  await expect(page.getByRole('button', { name: 'Retry reference save', exact: true })).toBeVisible();
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
  await page.getByRole('button', { name: 'Retry reference save', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Remove reference second.txt' })).toBeVisible();
  const imports = await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[]; requests: unknown[] } }).dropFixture);
  expect(imports.imports.map(source => source.split('/').at(-1))).toEqual(['first.txt', 'first.txt', 'second.txt']);
  expect(imports.requests).toEqual([]);
  await expect(page.locator('#track-A')).toHaveAttribute('aria-selected', 'true');
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Use these references');
  await page.getByRole('button', { name: 'Put', exact: false }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const requests = await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { requests: unknown[] } }).dropFixture.requests);
  expect(requests).toHaveLength(1);
  expect(requests[0]).toMatchObject({ target: { mode: 'lock', trackId: 'B' }, selectedTrackId: 'A', attachmentIds: ['first-reference', 'second-reference'] });
});

async function seedCard(kind: 'worker' | 'approval' | 'report' | 'request' | 'assumption' | 'unbound') {
  await app.evaluate((_, kind) => {
    const state = (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus } }).dropFixture.state;
    const worker: WorkerState = { trackId: 'B', phase: 'ready', sessionId: 'worker-B', activeTurn: null, identity: null, detail: null, checkpointPending: false, checkpoint: null, prompts: [], approvals: [], output: null };
    if (kind === 'approval') {
      worker.phase = 'approval';
      worker.approvals = [{ id: 'permission-B', requirement: { sourceIndex: 0 }, subject: 'git push origin main', reason: { kind: 'push', message: 'Review the push' }, canAllow: true, canDeny: true, replyMode: 'feedback' }];
    }
    state.workers = [worker];
    if (kind === 'report') state.reportQuestions = [{ id: 'report-B', reportId: 'report', trackId: 'B', sessionId: 'worker-B', turnId: 'turn-B', revision: 1, kind: 'question', text: 'Which reference should I use?', state: 'open', replyUtteranceId: null }];
    if (kind === 'request' || kind === 'unbound') state.utterances = [{ id: 'request-B', text: 'Use the reference for this task', attachments: [], createdAt: 1, state: 'asked', revision: 1, target: kind === 'request' ? { mode: 'lock', trackId: 'B' } : { mode: 'none' }, decision: null, reason: 'Confirm the instruction' }];
    if (kind === 'assumption') state.assumptions = [{ id: 'assumption-B', source: 'default-placement', sourceId: 'default', trackId: 'B', clipId: 'clip-B', question: 'Which format?', options: ['Text', 'JSON'], text: 'Text', evidence: 'I assumed plain text.', revision: 1, state: 'open', keptAt: null, changedAt: null, changeInputId: null }];
    state.sequence = (state.sequence ?? 0) + 1;
  }, kind);
  return kind === 'worker' ? '[data-worker-card="B"]' : `[data-attention-card="${{ approval: 'approval-B-permission-B', report: 'report-report-B', request: 'request-request-B', unbound: 'request-request-B', assumption: 'assumption-assumption-B' }[kind]}"]`;
}

for (const kind of ['worker', 'approval', 'report', 'request', 'assumption'] as const) for (const zoom of [1, 2]) {
  test(`${kind} card drop keeps the draft and chooses its worker at ${zoom * 100}%`, async () => {
    const page = await app.firstWindow();
    const selector = await seedCard(kind);
    const selected = kind === 'assumption' ? 'B' : 'A';
    await page.locator('#track-' + selected).click();
    const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill('Keep my existing words');
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(zoom); }, zoom);
    await selectFiles(page, ['first.txt']);
    await dispatchDrop(page, selector, 'dragover');
    await expect(page.locator(selector)).toHaveClass(/file-drop-active/);
    await expect(page.getByText('Drop to attach · next words to Worker B', { exact: true })).toBeVisible();
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('drop.png'), Buffer.from(pixels, 'base64'));
    await expect(page.getByText('Drop to attach · next words to Worker B', { exact: true })).toBeInViewport({ ratio: 1 });
    await dispatchDrop(page, selector, 'drop');
    await expect(draft).toHaveValue('Keep my existing words');
    await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
    await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
    await expect(page.locator('#track-' + selected)).toHaveAttribute('aria-selected', 'true');
    expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { requests: unknown[] } }).dropFixture.requests)).toEqual([]);
  });
}

test('a card in the stage popover accepts a reference without answering the card', async () => {
  const page = await app.firstWindow();
  const selector = await seedCard('report');
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Next instruction');
  await page.keyboard.press('Meta+Shift+T');
  await page.getByRole('button', { name: 'Needs you · 1', exact: true }).click();
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, selector, 'drop');
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus } }).dropFixture.state.reportQuestions![0].replyUtteranceId)).toBeNull();
});

test('arriving questions keep the card under a file drag in its held order', async () => {
  const page = await app.firstWindow();
  const selector = await seedCard('report');
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, selector, 'dragover');
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus } }).dropFixture.state;
    state.reportQuestions!.unshift({ ...state.reportQuestions![0], id: 'new-question', trackId: 'A' });
    state.sequence!++;
  });
  await expect(page.getByText('+1 new', { exact: true })).toBeVisible();
  await expect(page.locator('[data-attention-card="report-new-question"]')).toBeHidden();
  await expect(page.locator(selector)).toHaveClass(/file-drop-active/);
  await dispatchDrop(page, selector, 'drop');
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
});

test('an import stays with its original draft when a reply is opened meanwhile', async () => {
  const page = await app.firstWindow();
  const selector = await seedCard('report');
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Original instruction');
  await selectFiles(page, ['first.txt']);
  await app.evaluate(() => { (globalThis as typeof globalThis & { dropFixture: { holdImport: boolean } }).dropFixture.holdImport = true; });
  await dispatchDrop(page, selector, 'drop');
  await page.getByRole('group', { name: 'Question from Worker B' }).getByRole('button', { name: 'Reply', exact: true }).click();
  await app.evaluate(() => { (globalThis as typeof globalThis & { dropFixture: { release: () => void } }).dropFixture.release(); });
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('');
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toHaveCount(0);
  await page.getByRole('button', { name: 'Cancel reply', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Original instruction');
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
});

test('a failed drop waits for its original session before retrying', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus; failFirst: boolean } }).dropFixture;
    fixture.state.session = { id: 'first-session', name: 'First session', slug: 'first', createdAt: 1 };
    fixture.state.sequence = (fixture.state.sequence ?? 0) + 1;
    fixture.failFirst = true;
  });
  await expect(page.getByRole('heading', { name: 'First session', exact: true })).toBeVisible();
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('First session words');
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, '#track-B', 'drop');
  await expect(page.getByRole('button', { name: 'Retry reference save', exact: true })).toBeVisible();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus } }).dropFixture.state;
    state.session = { id: 'second-session', name: 'Second session', slug: 'second', createdAt: 2 }; state.sequence!++;
  });
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('');
  await page.getByRole('button', { name: 'Retry reference save', exact: true }).click();
  await expect(page.locator('.toast')).toHaveText('Return to the original session to retry these reference files.');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[] } }).dropFixture.imports.length)).toBe(1);
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus } }).dropFixture.state;
    state.session = { id: 'first-session', name: 'First session', slug: 'first', createdAt: 1 }; state.sequence!++;
  });
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('First session words');
  await page.getByRole('button', { name: 'Retry reference save', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
  await page.reload();
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
});

test('a card without a destination cannot receive files', async () => {
  const page = await app.firstWindow();
  const selector = await seedCard('unbound');
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, selector, 'dragover');
  await expect(page.locator(selector)).not.toHaveClass(/file-drop-active/);
  await dispatchDrop(page, selector, 'drop');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[] } }).dropFixture.imports)).toEqual([]);
});

test('an answered card held in place cannot redirect a new attachment', async () => {
  const page = await app.firstWindow();
  const selector = await seedCard('approval');
  await page.locator(selector).hover();
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, selector, 'dragover');
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { dropFixture: { state: DaemonStatus } }).dropFixture.state;
    state.workers![0].approvals = []; state.sequence!++;
  });
  await expect(page.locator(selector)).toHaveAttribute('inert', '');
  await dispatchDrop(page, selector, 'drop');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[] } }).dropFixture.imports)).toEqual([]);
});

test('a reply draft keeps its contents when a file is dropped on another card', async () => {
  const page = await app.firstWindow();
  const selector = await seedCard('report');
  await page.getByRole('group', { name: 'Question from Worker B' }).getByRole('button', { name: 'Reply', exact: true }).click();
  const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
  await draft.fill('Reply text');
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, selector, 'dragover');
  await expect(page.locator(selector)).not.toHaveClass(/file-drop-active/);
  await dispatchDrop(page, selector, 'drop');
  await expect(draft).toHaveValue('Reply text');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[] } }).dropFixture.imports)).toEqual([]);
});

for (const kind of ['worker', 'assumption'] as const) test(`${kind} card leaves ordinary text drag events available to its controls`, async () => {
  const page = await app.firstWindow();
  const selector = await seedCard(kind);
  if (kind === 'assumption') await page.locator('#track-B').click();
  await expect(page.locator(selector)).toBeVisible();
  const accepted = await page.locator(selector).evaluate(element => {
    const transfer = new DataTransfer(); transfer.setData('text/plain', 'Dragged text');
    return element.dispatchEvent(new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: transfer }));
  });
  expect(accepted).toBe(true);
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[] } }).dropFixture.imports)).toEqual([]);
});

test('composer drop keeps its target and synthetic files cannot request arbitrary paths', async () => {
  const page = await app.firstWindow();
  await page.getByRole('combobox', { name: 'Request destination' }).selectOption('A');
  await selectFiles(page, ['first.txt']);
  await dispatchDrop(page, '.composer', 'dragover');
  await expect(page.getByText('Drop here to attach it to your next words')).toBeVisible();
  await dispatchDrop(page, '.composer', 'drop');
  await expect(page.getByRole('button', { name: 'Remove reference first.txt' })).toBeVisible();
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('A');
  const rejected = await page.evaluate(async () => {
    const fabricated = new File(['pretend'], '/private/arbitrary.txt', { type: 'text/plain' });
    Object.defineProperty(fabricated, 'path', { value: '/private/arbitrary.txt' });
    try { await window.intercat.attachDroppedReference(fabricated); return false; } catch { return true; }
  });
  expect(rejected).toBe(true);
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { dropFixture: { imports: string[] } }).dropFixture.imports)).toEqual([path.join(directory, 'first.txt')]);
});
