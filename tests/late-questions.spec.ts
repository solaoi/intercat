import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, ReportAnswer, Utterance } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication;
let directory: string;
test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/ic-late-question-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }) => {
    const state: DaemonStatus = {
      connected: true, sequence: 1, workerConfigured: true,
      session: { id: 'late-session', name: 'Late question fixture', slug: 'late-question', createdAt: 1 },
      tracks: [{ id: 'A', label: 'A', title: 'First task', status: 'working' }, { id: 'B', label: 'B', title: 'Independent task', status: 'working' }],
      workers: ['A', 'B'].map(trackId => ({ trackId, phase: 'working', sessionId: 'session-' + trackId, activeTurn: 'next-' + trackId, identity: null, detail: null, checkpointPending: false, checkpoint: null, approvals: [], prompts: [], output: null })),
      utterances: [],
      reportQuestions: [{ id: 'question', reportId: 'report', trackId: 'A', sessionId: 'session-A', turnId: 'old-A', revision: 1, kind: 'question', text: '保存先を選んでください。Which database should I use?', state: 'open', replyUtteranceId: null, askedBeforeLastMessage: true }],
    };
    const fixture = { state, replies: [] as { opId: string; questionId: string; revision: number; answer: ReportAnswer }[] };
    Object.assign(globalThis, { lateFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:report-response'); ipcMain.handle('intercat:report-response', (_event, request) => {
      fixture.replies.push(request);
      const question = state.reportQuestions![0];
      const word: Utterance = { id: request.opId, text: request.answer.text, attachments: [], createdAt: 1, state: 'placed', revision: 1, target: { mode: 'lock', trackId: 'A' }, replyTo: { id: question.id, revision: question.revision, question }, decision: { kind: 'extend', trackId: 'A', boundary: 'after-turn', command: null, confidence: 1, inferenceMs: 0, timedOut: false }, reason: null };
      state.utterances!.push(word);
      question.state = 'queued'; question.replyUtteranceId = word.id; state.sequence!++;
      return word;
    });
  });
});
test.afterEach(async () => {
  await closeWindow(app);
  await rm(directory, { recursive: true, force: true });
});

async function updateWorker(phase: 'working' | 'checkpoint' | 'question') {
  await app.evaluate((_electron, phase) => {
    const state = (globalThis as typeof globalThis & { lateFixture: { state: DaemonStatus } }).lateFixture.state;
    const worker = state.workers![0];
    worker.phase = phase;
    worker.activeTurn = phase === 'working' ? 'next-A' : null;
    worker.checkpointPending = phase === 'checkpoint';
    if (phase === 'question') worker.checkpoint = { turnId: 'next-A', sha: 'a'.repeat(40), reference: 'checkpoint', unmergedPaths: [], liveProcesses: false };
    state.sequence!++;
  }, phase);
}

test('late question preserves working, then stops the lane and keeps its reply bound to the old question', async () => {
  const page = await app.firstWindow();
  const row = page.getByRole('row').filter({ has: page.getByRole('gridcell', { name: /^A ·/ }) });
  const card = page.getByRole('group', { name: 'Question from Worker A', exact: true });
  await expect(row.getByRole('gridcell')).toHaveAttribute('aria-label', 'A · First task · working · next waits for your answer');
  await expect(row.getByText('next waits for your answer', { exact: true })).toBeVisible();
  await expect(card).toContainText('asked before your last message');
  await expect(page.getByRole('gridcell', { name: 'B · Independent task · working', exact: true })).toBeVisible();

  await app.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2);
  });
  const note = row.getByText('next waits for your answer', { exact: true });
  await note.scrollIntoViewIfNeeded(); await expect(note).toBeInViewport();
  const bounds = (await note.boundingBox())!; const lane = (await row.boundingBox())!;
  expect(bounds.y).toBeGreaterThanOrEqual(lane.y);
  expect(bounds.y + bounds.height).toBeLessThanOrEqual(lane.y + lane.height);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('late-question-200.png'), Buffer.from(pixels, 'base64'));

  await updateWorker('checkpoint');
  await expect(row.getByRole('gridcell')).toHaveAttribute('aria-label', 'A · First task · stopped');
  await expect(row.getByText('next waits for your answer', { exact: true })).toHaveCount(0);
  await page.reload();
  await expect(card).toContainText('asked before your last message');
  await updateWorker('question');
  await card.scrollIntoViewIfNeeded();
  await expect(card.getByRole('button', { name: 'Reply', exact: true })).toBeInViewport();
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const cardPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('late-card-200.png'), Buffer.from(cardPixels, 'base64'));
  await card.getByRole('button', { name: 'Reply', exact: true }).click();
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Use SQLite.');
  await page.getByRole('button', { name: 'Send reply', exact: true }).click();
  await expect(card).toContainText('Reply waiting · sends after this turn');
  await expect(row.getByRole('gridcell')).toHaveAttribute('aria-label', 'A · First task · stopped');
  const replies = await app.evaluate(() => (globalThis as typeof globalThis & { lateFixture: { replies: unknown[] } }).lateFixture.replies);
  expect(replies).toEqual([expect.objectContaining({ questionId: 'question', revision: 1, answer: { action: 'reply', text: 'Use SQLite.' } })]);
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { lateFixture: { state: DaemonStatus } }).lateFixture.state;
    state.reportQuestions = []; state.utterances![0].state = 'sent';
    state.workers![0].phase = 'working'; state.workers![0].activeTurn = 'reply-A'; state.sequence!++;
  });
  await expect(card).toHaveCount(0);
  await expect(row.getByRole('gridcell')).toHaveAttribute('aria-label', 'A · First task · working');
});

test('a timely question stops its idle lane without labelling it as a late question', async () => {
  const page = await app.firstWindow();
  await updateWorker('question');
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { lateFixture: { state: DaemonStatus } }).lateFixture.state;
    state.reportQuestions![0].askedBeforeLastMessage = false;
    state.workers![0].checkpoint!.turnId = 'old-A'; state.sequence!++;
  });
  await expect(page.getByRole('gridcell', { name: 'A · First task · stopped', exact: true })).toBeVisible();
  await expect(page.getByRole('gridcell', { name: 'B · Independent task · working', exact: true })).toBeVisible();
  await expect(page.getByText('asked before your last message', { exact: true })).toHaveCount(0);
  await expect(page.getByText('next waits for your answer', { exact: true })).toHaveCount(0);
});
