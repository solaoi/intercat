import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { ExecutionLimitsChange } from '../src/bridge';

for (const zoom of [1, 2]) test(`Worker limit retries its saved operation and validates input at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-execution-limits-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, tracks: [], workers: [], utterances: [] }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      let setting = { revision: 0, parallelWorkers: 4, plannerReviewerTurns: 2 };
      const calls: ExecutionLimitsChange[] = [];
      const saved = new Set<string>();
      Object.assign(globalThis, { workerCapacityCalls: calls });
      ipcMain.removeHandler('intercat:execution-limits');
      ipcMain.handle('intercat:execution-limits', () => setting);
      ipcMain.removeHandler('intercat:execution-limits-change');
      ipcMain.handle('intercat:execution-limits-change', (_event, request: ExecutionLimitsChange) => {
        calls.push(request);
        if (!saved.has(request.opId)) {
          if (request.revision !== setting.revision) throw new Error('Changed in another window.');
          saved.add(request.opId);
          setting = { revision: setting.revision + 1, parallelWorkers: request.parallelWorkers, plannerReviewerTurns: request.plannerReviewerTurns };
          if (calls.length === 1) throw new Error('Saved response was lost.');
        }
        return setting;
      });
      const window = BrowserWindow.getAllWindows()[0];
      window.setSize(1040, 760);
    }, zoom);
    const page = await app.firstWindow();
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const section = page.getByRole('region', { name: 'Parallel workers', exact: true });
    const input = section.getByRole('spinbutton', { name: 'Maximum running turns' });
    await expect(input).toHaveValue('4');
    const planner = section.getByRole('spinbutton', { name: 'Planner and Reviewer turns' });
    await expect(planner).toHaveValue('2');
    await input.fill('0');
    await expect(section.getByRole('button', { name: 'Apply execution limits', exact: true })).toBeDisabled();
    await input.fill('1');
    await planner.fill('1');
    await section.getByRole('button', { name: 'Apply execution limits', exact: true }).click();
    await expect(section.getByRole('alert')).toContainText('Saved response was lost.');
    await expect(input).toBeDisabled();
    await section.getByRole('button', { name: 'Retry execution limits', exact: true }).click();
    await expect(input).toBeEnabled();
    await expect(input).toHaveValue('1');
    await expect(planner).toHaveValue('1');
    await expect(section.getByRole('alert')).toHaveCount(0);
    const calls = await app.evaluate(() => (globalThis as typeof globalThis & { workerCapacityCalls: ExecutionLimitsChange[] }).workerCapacityCalls);
    await section.scrollIntoViewIfNeeded();
    expect(await section.evaluate(element => {
      const rect = element.getBoundingClientRect();
      return rect.left >= 0 && rect.right <= innerWidth;
    })).toBe(true);
    const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('parallel-workers.png'), Buffer.from(capture, 'base64'));
    expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
    await page.getByRole('button', { name: 'Close settings', exact: true }).click();
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(1));
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await expect(input).toHaveValue('1');
    await expect(planner).toHaveValue('1');
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});

for (const zoom of [1, 2]) test(`queue positions agree in request chips, worker details and timeline rows at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-worker-queue-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const window = BrowserWindow.getAllWindows()[0];
      window.setSize(1440, 1000);
      window.webContents.setZoomFactor(zoom);
      const tracks = ['A', 'B'].map(label => ({ id: label, label, title: `Dummy ${label}`, status: 'waiting', completion: null }));
      const workers = tracks.map(track => ({ trackId: track.id, phase: 'ready', sessionId: null, identity: null, activeTurn: null, approvals: [], prompts: [], checkpointPending: false, checkpoint: null, output: null, detail: null }));
      const utterances = tracks.map((track, index) => ({ id: `word-${track.id}`, text: `Dummy request ${track.label}`, target: { mode: 'lock', trackId: track.id }, selected: track.id, createdAt: index, state: 'placed', revision: 1, reason: null, attachments: [], decision: { kind: 'extend', trackId: track.id, boundary: 'after-turn', confidence: 1, inferenceMs: 1, timedOut: false, scores: {} } }));
      const scheduling = { limit: 1, active: 1, waiting: utterances.map((word, index) => ({ id: word.id, priority: 'user', position: index + 1 })) };
      Object.assign(globalThis, { queueFixture: { tracks, workers, utterances, scheduling } });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, tracks, workers, utterances, scheduling }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    }, zoom);
    const page = await app.firstWindow();
    await expect(page.locator('[data-utterance-id="word-A"]')).toContainText('to A · queued · 1st');
    await expect(page.locator('[data-utterance-id="word-B"]')).toContainText('to B · queued · 2nd');
    await expect(page.locator('.track-row').filter({ hasText: 'Dummy B' })).toContainText('queued · 2nd');
    await expect(page.locator('[data-worker-card="B"]')).toContainText('queued · 2nd');
    await app.evaluate(() => {
      const fixture = (globalThis as typeof globalThis & { queueFixture: { utterances: { id: string; state: string }[]; scheduling: { waiting: { id: string; position: number }[] } } }).queueFixture;
      fixture.utterances[0].state = 'cut';
      fixture.scheduling.waiting.splice(0, 1);
      fixture.scheduling.waiting[0].position = 1;
    });
    await expect(page.locator('[data-utterance-id="word-A"]')).toHaveCount(0);
    await expect(page.locator('[data-utterance-id="word-B"]')).toContainText('to B · queued · 1st');
    await expect(page.locator('.track-row').filter({ hasText: 'Dummy B' })).toContainText('queued · 1st');
    await expect(page.locator('[data-worker-card="B"]')).toContainText('queued · 1st');
    for (const [name, target] of [
      ['request', page.locator('[data-utterance-id="word-B"]')],
      ['timeline', page.locator('.track-row').filter({ hasText: 'Dummy B' })],
      ['worker', page.locator('[data-worker-card="B"]')],
    ] as const) {
      await target.scrollIntoViewIfNeeded();
      await expect(target).toBeInViewport({ ratio: 1 });
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(info.outputPath(`queue-${name}.png`), Buffer.from(capture, 'base64'));
    }
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
