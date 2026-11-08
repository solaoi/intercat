import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) test(`Finish a saved file update keeps the taken-back request at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-inheritance-ui-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }) => {
      const worker = { trackId: 'B', phase: 'held', sessionId: 'dummy-session', identity: null, activeTurn: null, approvals: [], prompts: [], checkpointPending: false, checkpoint: null, output: null, detail: 'A file update was interrupted.' };
      const pending = [{ operationId: '00000000-0000-5000-8000-000000000010', trackId: 'B', sourceInputId: 'dummy-cut', sourceState: 'cut', state: 'dispatched' }];
      const word = { id: 'dummy-cut', text: 'Dummy request taken back before it ran.', target: { mode: 'lock', trackId: 'B' }, selected: 'B', createdAt: 0, state: 'cut', revision: 2, reason: null, attachments: [], decision: { kind: 'extend', trackId: 'B', boundary: 'after-turn', confidence: 1, inferenceMs: 1, timedOut: false, scores: {} } };
      const state = { worker, pending, word, paused: false, calls: [] as { opId: string; trackId: string; applicationId: string }[], starts: 0 };
      Object.assign(globalThis, { inheritanceFixture: state });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: state.calls.length + 1, dispatchPaused: state.paused, tracks: [{ id: 'B', label: 'B', title: 'Dummy worker', status: 'waiting' }], workers: [worker], utterances: [word], pendingInheritance: pending }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:worker-start');
      ipcMain.handle('intercat:worker-start', () => { state.starts++; });
      ipcMain.removeHandler('intercat:inheritance-finish');
      ipcMain.handle('intercat:inheritance-finish', (_event, request) => {
        state.calls.push(request);
        if (state.calls.length === 1) throw new Error('Dummy disk unavailable.');
        worker.phase = 'held'; worker.detail = 'File update finished. No request was sent.';
        pending.splice(0);
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    });
    const page = await app.firstWindow();
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const finish = page.getByRole('button', { name: 'Finish file update', exact: true });
    await expect(finish).toBeEnabled();
    await expect(page.getByText('The request stays taken back.', { exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Start Worker B', exact: true })).toBeDisabled();
    await app.evaluate(() => { (globalThis as typeof globalThis & { inheritanceFixture: { paused: boolean } }).inheritanceFixture.paused = true; });
    await expect(finish).toBeEnabled();
    await finish.click();
    await expect(page.getByRole('alert').filter({ hasText: 'Dummy disk unavailable.' })).toBeVisible();
    await expect(finish).toBeEnabled();
    await finish.focus();
    const geometry = await finish.evaluate(element => { const r = element.getBoundingClientRect(); return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: innerWidth, height: innerHeight, hit: element.contains(document.elementFromPoint(r.left+r.width/2,r.top+r.height/2)) }; });
    expect(geometry.left).toBeGreaterThanOrEqual(0); expect(geometry.right).toBeLessThanOrEqual(geometry.width);
    expect(geometry.top).toBeGreaterThanOrEqual(0); expect(geometry.bottom).toBeLessThanOrEqual(geometry.height);
    expect(geometry.hit).toBe(true);
    const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('inheritance-recovery.png'), Buffer.from(capture, 'base64'));
    await finish.press('Enter');
    await expect(finish).toHaveCount(0);
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toBeFocused();
    const state = await app.evaluate(() => (globalThis as typeof globalThis & { inheritanceFixture: { word: { state: string }; calls: { applicationId: string; trackId: string }[]; starts: number; paused: boolean } }).inheritanceFixture);
    expect(state.word.state).toBe('cut'); expect(state.starts).toBe(0); expect(state.paused).toBe(true);
    expect(state.calls).toHaveLength(2);
    expect(state.calls.every(call => call.applicationId === '00000000-0000-5000-8000-000000000010' && call.trackId === 'B')).toBe(true);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
