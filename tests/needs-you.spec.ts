import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, InputResolution, Utterance, WorkerState } from '../src/bridge';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

let app: ElectronApplication;
let directory: string;
let page: Page;

const request = (id: string): Utterance => ({
  id, text: `Choose a destination for ${id}`, attachments: [], createdAt: 1,
  state: 'asked', revision: 1, target: { mode: 'none' }, decision: null,
  reason: 'Where should this request go?',
});
const worker: WorkerState = {
  trackId: 'A', phase: 'ready', sessionId: 'session-A', activeTurn: null, identity: null,
  detail: null, checkpointPending: false, checkpoint: null, approvals: [], prompts: [], output: null,
};

test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/ic-needs-you-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  page = await app.firstWindow();
  const now = Date.now();
  await page.clock.install({ time: now });
  await page.clock.pauseAt(now + 1000);
  await app.evaluate(({ ipcMain }, initial) => {
    const state: DaemonStatus = {
      connected: true, sequence: 1, workerConfigured: true,
      session: { id: 'cards-session', name: 'Card ordering', slug: 'cards', createdAt: 1 },
      tracks: [{ id: 'A', label: 'A', title: 'Arithmetic', status: 'ready' }],
      workers: [initial.worker], utterances: [initial.request],
    };
    const fixture = { state, resolutions: [] as { utteranceId: string; resolution: InputResolution }[] };
    Object.assign(globalThis, { cardsFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event: unknown, value: { utteranceId: string; resolution: InputResolution }) => {
      fixture.resolutions.push(value);
      const word = state.utterances!.find(word => word.id === value.utteranceId)!;
      word.state = 'held'; word.revision++; state.sequence!++;
      return word;
    });
  }, { worker, request: request('first') });
  await page.clock.runFor(1100);
  await expect(page.getByRole('region', { name: 'Needs you', exact: true })).toBeVisible();
  await page.mouse.move(5, 5);
  await page.clock.runFor(1600);
});

test.afterEach(async () => {
  await closeWindow(app);
  await rm(directory, { recursive: true, force: true });
});

async function update(change: Partial<DaemonStatus>) {
  await app.evaluate((_electron, change) => {
    const state = (globalThis as typeof globalThis & { cardsFixture: { state: DaemonStatus } }).cardsFixture.state;
    Object.assign(state, change); state.sequence = (state.sequence ?? 0) + 1;
  }, change);
  await page.clock.runFor(1100);
}
const card = (id: string) => page.locator(`[data-attention-card="${id}"]`);
const visibleCards = () => page.locator('[data-attention-card]:visible');

function approval() {
  return { ...worker, phase: 'approval' as const, approvals: [{ id: 'permission', requirement: { action: 'read' }, subject: 'Read a dummy file', reason: { kind: 'read', message: 'Read permission' }, canAllow: true, canDeny: true }] };
}

async function leave() {
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5);
  await page.clock.runFor(1600);
}

test('unstarted requests show the specified status and nonblocking output leaves them open', async () => {
  await expect(card('request-first')).toContainText('Not started · pick a track');
  await expect(page.locator('.request-strip .request-chip')).toHaveCount(0);
  await expect(card('request-first')).toContainText('Picking a track re-reads what to do.');
  await update({ reports: [{ id: 'pending', trackId: 'A', pending: true }], workers: [{ ...worker, output: 'Work is ready for review.' }] });
  await expect(card('request-first')).toBeVisible();
  await expect(page.getByText('Checking the report for questions…', { exact: false })).toBeVisible();
  await expect(visibleCards()).toHaveCount(1);
});

test('all pending requests remain reachable and answer their own IDs', async () => {
  await update({ utterances: [request('first'), request('second'), request('third')] });
  expect(await visibleCards().count()).toBeGreaterThanOrEqual(1);
  expect(await visibleCards().count()).toBeLessThanOrEqual(2);
  await expect(card('request-first')).toBeVisible();
  await expect(card('request-third')).toBeHidden();
  await page.getByRole('button', { name: /\+\d+ more/ }).click();
  await expect(visibleCards()).toHaveCount(3);
  await card('request-third').getByRole('button', { name: 'Worker A', exact: true }).click();
  await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { cardsFixture: { resolutions: unknown[] } }).cardsFixture.resolutions)).toEqual([expect.objectContaining({ utteranceId: 'third', resolution: { action: 'retry', target: { mode: 'lock', trackId: 'A' } } })]);
});

test('a stopped approval folds the editor question and puts the answer nearest the composer', async () => {
  await update({ workers: [approval()] });
  await expect(card('request-first')).toBeHidden();
  await expect(card('approval-A-permission')).toBeVisible();
  await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(card('request-first')).toBeVisible();
  const question = (await card('request-first').boundingBox())!;
  const permission = (await card('approval-A-permission').boundingBox())!;
  expect(question.y + question.height).toBeLessThanOrEqual(permission.y);
  await card('approval-A-permission').hover();
  const changed = approval();
  changed.approvals[0].requirement = { action: 'write' };
  changed.approvals[0].subject = 'Write a different dummy file';
  await update({ workers: [changed] });
  await expect(card('approval-A-permission')).toContainText('changed elsewhere');
  await expect(card('approval-A-permission')).toContainText('Read a dummy file');
  await expect(card('approval-A-permission').locator(':scope > fieldset')).toHaveAttribute('aria-hidden', 'true');
  await leave();
  await expect(card('approval-A-permission')).toContainText('Write a different dummy file');

});

for (const phase of ['lost', 'needs-recovery', 'after-connection-loss'] as const) {
  test(`${phase} folds an editor question until recovery is shown`, async () => {
    await update({ workers: [{ ...worker, phase, detail: 'Recovery needs your choice.' }] });
    await expect(card('request-first')).toBeHidden();
    await expect(card('worker-A')).toBeVisible();
    await expect(page.getByRole('button', { name: /\+1 more/ })).toBeVisible();
    await update({ workers: [worker] });
    await expect(card('request-first')).toBeVisible();
  });
}

test('hover holds layout through urgent arrivals and releases after one and a half seconds', async () => {
  await card('request-first').hover();
  await update({ workers: [approval()] });
  await expect(card('request-first')).toBeVisible();
  await expect(card('approval-A-permission')).toBeHidden();
  await expect(page.getByText('+1 new', { exact: true })).toBeVisible();
  await page.mouse.move(5, 5);
  await page.clock.runFor(1499);
  await expect(card('request-first')).toBeVisible();
  await page.clock.runFor(2);
  await expect(card('request-first')).toBeHidden();
  await expect(card('approval-A-permission')).toBeVisible();
});

test('an answered card becomes inert and focus moves to the next card before cleanup', async () => {
  await update({ utterances: [request('first'), request('second')] });
  if (!await card('request-second').isVisible()) await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(card('request-second')).toBeVisible();
  const relativeTop = () => card('request-second').evaluate(element => element.getBoundingClientRect().top - element.closest('.needs-you')!.getBoundingClientRect().top);
  const secondTop = await relativeTop();
  await card('request-first').getByRole('button', { name: 'Worker A', exact: true }).focus();
  await update({ utterances: [request('second')] });
  await writeFile(test.info().outputPath('focus-state.json'), JSON.stringify(await page.evaluate(() => ({ active: document.activeElement?.outerHTML.slice(0, 150), cards: [...document.querySelectorAll<HTMLElement>('[data-attention-card]')].map(card => ({ id: card.dataset.attentionCard, hidden: card.hidden, inert: card.inert })) })), null, 2));
  await expect(card('request-first')).toContainText('answered elsewhere');
  await expect(card('request-first').locator(':scope > fieldset')).toHaveAttribute('aria-hidden', 'true');
  await expect(card('request-first').getByRole('button', { name: 'Worker A', exact: true, includeHidden: true })).toBeDisabled();
  await expect(card('request-second').getByRole('button', { name: 'Worker A', exact: true })).toBeFocused();
  expect(Math.abs(await relativeTop() - secondTop)).toBeLessThan(1);
  await leave();
  await expect(card('request-first')).toHaveCount(0);
});

test('folding a worker question preserves its selected answer', async () => {
  const prompt = { request: { sessionId: 'session-A', userInputId: 'choice', turnId: 'turn-A', questions: [{ id: 'q', header: 'Database', question: 'Choose a database', selection: { mode: 'single' as const }, options: [{ label: 'SQLite' }, { label: 'Other database' }] }] }, revision: '1', phase: 'open' as const, operation: null };
  await update({ utterances: [], workers: [{ ...worker, phase: 'question', prompts: [prompt] }] });
  await page.getByRole('radio', { name: 'SQLite', exact: true }).check();
  await leave();
  await page.keyboard.press('Meta+Shift+T');
  await page.clock.runFor(50);
  await page.getByRole('button', { name: 'Needs you · 1', exact: true }).click();
  await expect(page.getByRole('radio', { name: 'SQLite', exact: true })).toBeChecked();
  await page.keyboard.press('Escape');
  await page.keyboard.press('Meta+Shift+T');
  await page.clock.runFor(50);
  await expect(page.getByRole('radio', { name: 'SQLite', exact: true })).toBeChecked();
  await leave();
  const questions = [0, 1].map(index => ({ id: 'report-' + index, reportId: 'report', trackId: 'A', sessionId: 'session-A', turnId: 'turn', revision: 1, kind: 'question' as const, text: 'Another question', state: 'open' as const, replyUtteranceId: null }));
  await update({ reportQuestions: questions });
  await expect(card('prompt-A-choice')).toBeHidden();
  await update({ reportQuestions: [] });
  await expect(card('prompt-A-choice')).toBeVisible();
  await expect(page.getByRole('radio', { name: 'SQLite', exact: true })).toBeChecked();
});

test('long Japanese questions and their actions remain reachable at two hundred percent', async () => {
  const text = 'この変更をどのワーカーに送るか選択してください。'.repeat(30);
  await update({ utterances: [{ ...request('first'), text }] });
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  await card('request-first').getByRole('button', { name: 'Worker A', exact: true }).scrollIntoViewIfNeeded();
  await expect(card('request-first').getByText('Not started · pick a track', { exact: true })).toBeVisible();
  await expect(card('request-first').getByRole('button', { name: 'Worker A', exact: true })).toBeInViewport();
  const actions = (await card('request-first').locator('[data-card-actions]').boundingBox())!;
  const scroll = (await page.locator('.desk-scroll').boundingBox())!;
  expect(actions.y).toBeGreaterThanOrEqual(scroll.y);
  expect(actions.y + actions.height).toBeLessThanOrEqual(scroll.y + scroll.height);
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('questions-200.png'), Buffer.from(pixels, 'base64'));
});


test('removing the final focused card returns focus to the request draft', async () => {
  await card('request-first').getByRole('button', { name: 'Worker A', exact: true }).focus();
  await update({ utterances: [] });
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeFocused();
  await page.mouse.move(5, 5);
  await page.clock.runFor(1600);
  await expect(card('request-first')).toHaveCount(0);
});

test('a new version of a worker question requires a new answer', async () => {
  const prompt = { request: { sessionId: 'session-A', userInputId: 'choice', turnId: 'turn-A', questions: [{ id: 'q', header: 'Database', question: 'Choose a database', selection: { mode: 'single' as const }, options: [{ label: 'SQLite' }, { label: 'Other database' }] }] }, revision: '1', phase: 'open' as const, operation: null };
  await update({ utterances: [], workers: [{ ...worker, phase: 'question', prompts: [prompt] }] });
  await page.getByRole('radio', { name: 'SQLite', exact: true }).check();
  await update({ workers: [{ ...worker, phase: 'question', prompts: [{ ...prompt, revision: '2', request: { ...prompt.request, questions: [{ ...prompt.request.questions[0], question: 'Choose the reporting database' }] } }] }] });
  await expect(card('prompt-A-choice')).toContainText('changed elsewhere');
  await expect(card('prompt-A-choice').locator(':scope > fieldset')).toHaveAttribute('aria-hidden', 'true');
  await leave();
  await expect(page.getByText('Choose the reporting database', { exact: true })).toBeVisible();
  await expect(page.getByRole('radio', { name: 'SQLite', exact: true })).not.toBeChecked();
  await expect(page.getByRole('button', { name: 'Send answers', exact: true })).toBeDisabled();
});


test('a resolved Editor card closes its details dialog immediately', async () => {
  await update({ utterances: [], editor: { phase: 'unavailable', process: false, accepting: false, modelLoaded: false, generation: 1, restart: 3, canRetry: true, detail: 'The dummy Editor needs attention.' } });
  await page.getByRole('button', { name: 'Details', exact: true }).click();
  await expect(page.getByRole('dialog', { name: 'Editor details', exact: true })).toBeVisible();
  await update({ editor: { phase: 'ready', process: true, accepting: true, modelLoaded: true, generation: 2, restart: 0, canRetry: false } });
  await expect(page.getByRole('dialog', { name: 'Editor details', exact: true })).toHaveCount(0);
});
