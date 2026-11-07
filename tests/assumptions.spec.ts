import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import type { Assumption, AssumptionCommand, DaemonStatus } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication;
let page: Page;
let directory: string;
const choice: Assumption = {
  id: 'db-choice', source: 'default-placement', sourceId: 'source', clipId: 'clip-A', trackId: 'A',
  question: 'Which database should the local cache use?', options: ['SQLite', 'PostgreSQL'],
  text: 'SQLite', evidence: 'I assumed SQLite and continued implementing the cache.',
  revision: 1, state: 'open', keptAt: null, changedAt: null, changeInputId: null,
};

test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/ic-assumptions-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  page = await app.firstWindow();
  await app.evaluate(({ ipcMain }, choice) => {
    const state: DaemonStatus = {
      connected: true, sequence: 1, workerConfigured: true, editor: { phase: 'ready', process: true, accepting: true, modelLoaded: true, generation: 1, restart: 0, canRetry: false },
      tracks: [{ id: 'A', label: 'A', title: 'Local cache', status: 'working' }, { id: 'B', label: 'B', title: 'Other task', status: 'ready' }],
      workers: [], assumptions: [choice], utterances: [],
    };
    const fixture = { state, commands: [] as AssumptionCommand[], loseReply: false };
    Object.assign(globalThis, { assumptionFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:assumption-answer'); ipcMain.handle('intercat:assumption-answer', (_event, command: AssumptionCommand) => {
      fixture.commands.push(command);
      if (fixture.loseReply) { fixture.loseReply = false; throw new Error('Acknowledgement lost'); }
      state.assumptions = []; state.sequence!++;
      return { ...choice, state: command.answer.action === 'keep' ? 'kept' : 'changed' };
    });
  }, choice);
  await expect(page.locator('[data-attention-card="assumption-db-choice"]')).toBeVisible();
});

test.afterEach(async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); });

const card = () => page.locator('.assumption-card');

test('an assumption is collapsed on its own track and leaves an editor question open', async () => {
  await expect(card()).not.toHaveAttribute('open');
  await expect(card()).toContainText('SQLite');
  await card().locator('summary').click();
  const keep = card().getByRole('button', { name: 'Keep', exact: true });
  await expect(keep).toBeVisible();
  expect(await keep.evaluate(element => getComputedStyle(element).color)).toBe('rgb(201, 203, 209)');
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { assumptionFixture: { state: DaemonStatus } }).assumptionFixture.state;
    state.utterances = [{ id: 'request', text: 'Make it faster', attachments: [], createdAt: 1, state: 'asked', revision: 1, target: { mode: 'none' }, decision: null, reason: 'Choose a destination.' }];
    state.sequence!++;
  });
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5);
  await expect(page.locator('[data-attention-card="request-request"]')).toBeVisible();
  if (!await card().isVisible()) await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(card()).toBeVisible();
  await page.getByRole('gridcell', { name: 'B · Other task · waiting', exact: true }).click();
  await expect(card()).toHaveCount(0);
});

test('a lost acknowledgement retries the same Keep operation and keeps its answer locked', async () => {
  await card().locator('summary').click();
  await app.evaluate(() => { (globalThis as typeof globalThis & { assumptionFixture: { loseReply: boolean } }).assumptionFixture.loseReply = true; });
  await card().getByRole('button', { name: 'Keep', exact: true }).click();
  await expect(card().getByRole('alert')).toContainText('Acknowledgement lost');
  await expect(card().getByRole('button', { name: 'Change', exact: true })).toHaveCount(0);
  await card().getByRole('button', { name: 'Retry saving answer', exact: true }).click();
  const commands = await app.evaluate(() => (globalThis as typeof globalThis & { assumptionFixture: { commands: AssumptionCommand[] } }).assumptionFixture.commands);
  expect(commands).toHaveLength(2);
  expect(commands[0]).toEqual(commands[1]);
  expect(commands[0].answer).toEqual({ action: 'keep' });
});

test('Change submits the selected alternative only after explicit confirmation', async () => {
  await card().locator('summary').click();
  await card().getByRole('button', { name: 'Change', exact: true }).click();
  await expect(card().getByRole('textbox', { name: 'Change assumption', exact: true })).toHaveValue('PostgreSQL');
  await card().getByRole('textbox', { name: 'Change assumption', exact: true }).fill('Use PostgreSQL. Keep the public API.');
  await card().getByRole('button', { name: 'Send correction', exact: true }).click();
  const commands = await app.evaluate(() => (globalThis as typeof globalThis & { assumptionFixture: { commands: AssumptionCommand[] } }).assumptionFixture.commands);
  expect(commands).toHaveLength(1);
  expect(commands[0]).toMatchObject({ assumptionId: choice.id, revision: 1, answer: { action: 'change', text: 'Use PostgreSQL. Keep the public API.' } });
});

test('closing a question-origin clip preserves a correction draft', async () => {
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { assumptionFixture: { state: DaemonStatus } }).assumptionFixture;
    fixture.state.assumptions![0].questionId = 'original-question';
    fixture.state.sequence!++;
  });
  await card().locator('summary').click();
  await expect(card()).toContainText('Worker asked ·');
  await card().getByRole('button', { name: 'Change', exact: true }).click();
  const correction = card().getByRole('textbox', { name: 'Change assumption', exact: true });
  await correction.fill('Use PostgreSQL with the existing connection pool.');
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { assumptionFixture: { state: DaemonStatus } }).assumptionFixture;
    fixture.state.assumptions![0].source = 'question-timeout';
    fixture.state.tracks![0].title = 'Closed report';
    fixture.state.sequence!++;
  });
  await expect(page.getByRole('gridcell', { name: /^A · Closed report/ })).toBeVisible();
  await expect(correction).toHaveValue('Use PostgreSQL with the existing connection pool.');
});

test('the expanded card and its action row fit at 200 percent', async () => {
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  await card().locator('summary').click();
  await card().getByRole('button', { name: 'Change', exact: true }).click();
  const send = card().getByRole('button', { name: 'Send correction', exact: true });
  await send.scrollIntoViewIfNeeded();
  await expect(send).toBeInViewport();
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeInViewport();
  const bounds = (await card().boundingBox())!;
  const scroll = (await page.locator('.desk-scroll').boundingBox())!;
  expect(bounds.x).toBeGreaterThanOrEqual(scroll.x);
  expect(bounds.x + bounds.width).toBeLessThanOrEqual(scroll.x + scroll.width);
});
