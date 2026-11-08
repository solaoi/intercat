import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { DaemonStatus, EditAction, TimelineClip, Utterance } from '../src/bridge';
import type { Plan } from '../src/plans/types';

for (const zoom of [1, 2]) test(`Read as selects an exact clip or plan wait at ${zoom * 100}%`, async ({}, info) => {
  const root = await mkdtemp('/private/tmp/ic-wait-targets-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + root] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const plan: Plan = { id: 'plan', rev: 7, state: 'draft', sourceId: 'source', sourceRevision: 1, executeAllowed: false, lockedTrack: null, startMode: 'manual', generation: { id: 'generation', state: 'ready', attempt: 0, detail: null }, basedOn: [], attachments: [], summary: 'Dummy waiting plan', assumptions: [], notices: [], seenRevision: null, rewrite: null, items: [{ planItemId: 'planned', trackRef: 'A', folderId: 'dummy', order: 0, title: 'Prepare dummy index', instruction: 'Dummy only', constraints: [], doneWhen: 'Dummy exists', after: [], state: 'planned', skipOrder: [], startRequested: false, utteranceId: null, detail: null }] };
      const word: Utterance = { id: 'request', text: 'Send this dummy request to B', createdAt: 1, attachments: [], state: 'placed', revision: 1, target: { mode: 'lock', trackId: 'B' }, decision: { kind: 'schedule', trackId: 'B', boundary: 'when-told', command: null, confidence: 1, inferenceMs: 0, timedOut: false }, reason: null };
      const clips: TimelineClip[] = [true, false].map((running, i) => ({ id: `clip-${i}`, trackId: 'A', repairRunId: null, ordinal: 14 + i, sessionId: 'session-a', turnId: `turn-${i}`, category: 'step', label: running ? 'Dummy current work' : 'Dummy finished work', kind: 'think', start: i * 1000, end: (i + 1) * 1000, running, generationEnd: null, approximate: false, toolTicks: [] }));
      const state: DaemonStatus = { connected: true, workerConfigured: true, sequence: 1, plans: [plan], utterances: [word], tracks: [{ id: 'A', label: 'A', title: 'Dummy A', status: 'working' }, { id: 'B', label: 'B', title: 'Dummy B', status: 'done' }], workers: [{ trackId: 'A', phase: 'working', activeTurn: 'turn-0', sessionId: 'session-a', approvals: [], prompts: [], checkpointPending: false, checkpoint: null, identity: null, detail: null, output: null }] };
      const fixture = { state, clips, calls: [] as EditAction[], receipts: new Set<string>(), lose: true };
      Object.assign(globalThis, { waitsFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips, marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:edit-request'); ipcMain.handle('intercat:edit-request', (_event, action: EditAction) => {
        fixture.calls.push(action);
        if (action.kind === 'begin') { word.revision++; word.editHold = { owner: 'window', token: action.token, revision: word.revision }; }
        if (action.kind === 'apply' && !fixture.receipts.has(action.opId)) {
          if (action.revision !== word.revision || word.editHold?.token !== action.token) throw new Error('Stale edit');
          const edit = action.edit;
          if (edit.kind === 'waitFor') {
            word.decision!.boundary = 'after-turn';
            word.startCondition = { source: edit.source.kind === 'clip' ? { kind: 'clip', value: { id: edit.source.clipId, ordinal: 14, turn: { trackId: 'A', label: 'A', sessionId: 'session-a', turnId: 'turn-0' } } } : { kind: 'plan-item', value: { planId: 'plan', itemId: 'planned', title: 'Prepare dummy index', label: 'A' } }, candidates: [], ready: false };
          } else if (edit.kind === 'readAs') { word.decision!.boundary = edit.boundary; word.startCondition = null; }
          word.revision++; word.editHold!.revision = word.revision; fixture.receipts.add(action.opId); state.sequence!++;
          if (fixture.lose) { fixture.lose = false; throw new Error('Dummy acknowledgement lost'); }
        }
        if (action.kind === 'end') { word.editHold = null; state.sequence!++; }
        return structuredClone(word);
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760); BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow();
    await page.locator('.request-select').click();
    const popup = page.getByRole('dialog', { name: 'Request', exact: true });
    const chooser = popup.getByRole('combobox', { name: 'Wait for work', exact: true });
    await expect(chooser).toBeEnabled();
    await expect(chooser.locator('option', { hasText: 'Dummy current work' })).toHaveCount(1);
    await expect(chooser.locator('option', { hasText: 'Prepare dummy index' })).toHaveCount(1);
    await expect(chooser.locator('option')).toHaveCount(3);
    await expect(chooser.locator('option', { hasText: 'Dummy finished work' })).toHaveCount(0);
    await chooser.selectOption('clip:clip-0');
    await expect(popup).toContainText('Dummy acknowledgement lost');
    await popup.getByRole('button', { name: 'Retry change', exact: true }).click();
    await expect(popup).toContainText('Send to Worker B · after the turn containing A:14');
    const attempts = await app.evaluate(() => (globalThis as any).waitsFixture.calls.filter((call: EditAction) => call.kind === 'apply'));
    expect(attempts).toHaveLength(2); expect(attempts[0]).toEqual(attempts[1]);
    await chooser.focus(); await expect(chooser).toBeInViewport();
    await writeFile(info.outputPath('clip-wait.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await chooser.selectOption('plan:planned');
    await expect(popup).toContainText('Send to Worker B · after A · Prepare dummy index');
    expect(await popup.getByRole('combobox', { name: 'Edited request destination' }).inputValue()).toBe('B');
    await popup.getByRole('button', { name: 'Close request details', exact: true }).click();
    await expect(page.locator('.request-select')).toContainText('after A · Prepare dummy index');
    await app.evaluate(() => { const fixture = (globalThis as any).waitsFixture; fixture.state.utterances[0].startCondition.detail = 'Dependency did not finish'; fixture.state.sequence++; });
    await expect(page.locator('.request-select')).toContainText('held · Dependency did not finish');
    await page.locator('.request-select').click();
    await expect(popup).toContainText('change the wait target or start time');
    await popup.getByRole('radio', { name: 'When I say', exact: true }).check();
    await expect(popup).toContainText('Send to Worker B · when I say');
    await popup.getByRole('button', { name: 'Close request details', exact: true }).click();
    await expect(page.locator('.request-select')).toContainText('waiting for your go');
    const calls = await app.evaluate(() => (globalThis as any).waitsFixture.calls);
    const planEdit = calls.find((call: EditAction) => call.kind === 'apply' && call.edit.kind === 'waitFor' && call.edit.source.kind === 'plan-item');
    expect(planEdit.edit.source).toEqual({ kind: 'plan-item', planId: 'plan', itemId: 'planned', revision: 7 });
    await writeFile(info.outputPath('waits.json'), JSON.stringify(calls, null, 2));
  } finally { await closeWindow(app); await rm(root, { recursive: true, force: true }); }
});
