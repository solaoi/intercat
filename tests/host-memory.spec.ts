import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) test(`memory waiting agrees in requests, workers and timeline at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-host-memory-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const fixture = {
        connected: true, sequence: 1,
        tracks: [{ id: 'A', label: 'A', title: 'Dummy memory test', status: 'waiting', completion: null }],
        workers: [{ trackId: 'A', phase: 'waiting-memory', sessionId: null, identity: null, activeTurn: null, approvals: [], prompts: [], checkpointPending: false, checkpoint: null, output: null, detail: 'queued · waiting for memory' }],
        utterances: [{ id: 'word-A', text: 'Dummy request', target: { mode: 'lock', trackId: 'A' }, selected: 'A', createdAt: 1, state: 'placed', revision: 1, reason: null, attachments: [], decision: { kind: 'extend', trackId: 'A', boundary: 'now', confidence: 1, inferenceMs: 1, timedOut: false, scores: {} } }],
        hostMemory: { sample: { freeBytes: 0, pressured: true }, error: null, estimatedHostBytes: 272629760, hosts: 0, waiting: [{ owner: 'A', position: 1 }] },
      };
      Object.assign(globalThis, { memoryFixture: fixture, memoryStops: 0 });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => fixture);
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:worker-stop');
      ipcMain.handle('intercat:worker-stop', () => { Object.assign(globalThis, { memoryStops: 1 }); fixture.workers[0].phase = 'sleeping'; fixture.hostMemory.waiting = []; fixture.utterances[0].state = 'held'; });
      BrowserWindow.getAllWindows()[0].setSize(1100, 800);
      BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow();
    await expect(page.locator('[data-utterance-id="word-A"]')).toContainText('queued · waiting for memory');
    await expect(page.locator('[data-worker-card="A"]')).toContainText('queued · waiting for memory');
    await expect(page.locator('.track-row').filter({ hasText: 'Dummy memory test' })).toContainText('queued · waiting for memory');
    await expect(page.getByRole('button', { name: 'Start Worker A', exact: true })).toBeDisabled();
    await page.getByRole('button', { name: 'Stop Worker A', exact: true }).click();
    await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { memoryStops: number }).memoryStops)).toBe(1);
    await expect(page.locator('[data-worker-card="A"]')).toContainText('ready');
    await expect(page.getByRole('button', { name: 'Start Worker A', exact: true })).toBeEnabled();
    await expect(page.locator('[data-utterance-id="word-A"]')).not.toContainText('waiting for memory');
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
