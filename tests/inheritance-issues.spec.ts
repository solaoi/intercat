import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const kind of ['processes', 'conflict'] as const) for (const zoom of [1, 2]) test(`${kind} inheritance card preserves the held work at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-inheritance-issue-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, kind) => {
      const issue = { id: '00000000-0000-7000-8000-000000000012', input: 'dummy-word', trackId: 'B', sessionId: kind === 'processes' ? 'dummy-session' : null, revision: 2, parents: ['A'], applicationId: kind === 'processes' ? '00000000-0000-5000-8000-000000000013' : null, blocker: kind === 'processes' ? { kind } : { kind, paths: ['src/dummy.ts'], base: false }, kept: false, planId: 'dummy-plan', itemId: 'dummy-item' };
      const worker = { trackId: 'B', phase: 'held', sessionId: 'dummy-session', identity: null, activeTurn: null, approvals: [], prompts: [], checkpointPending: false, checkpoint: null, output: null, detail: null };
      const word = { id: 'dummy-word', text: 'Dummy held request.', target: { mode: 'lock', trackId: 'B' }, selected: 'B', createdAt: 0, state: 'held', revision: 2, reason: 'Inherited files need review', attachments: [], decision: { kind: 'extend', trackId: 'B', boundary: 'after-turn', confidence: 1, inferenceMs: 1, timedOut: false, scores: {} } };
      const plan = { id: 'dummy-plan', rev: 2, state: 'running', sourceId: 'source', sourceRevision: 1, startMode: 'manual', executeAllowed: false, lockedTrack: null, generation: { id: 'generation', state: 'ready', attempt: 0, detail: null }, basedOn: [], attachments: [], summary: 'Dummy conflict plan', assumptions: [], notices: [], seenRevision: null, items: [{ planItemId: 'dummy-item', trackRef: 'B', folderId: 'dummy-folder', order: 0, title: 'Dummy held job', instruction: 'Use dummy data only.', constraints: [], doneWhen: 'Dummy job finished.', after: [], state: 'planned', utteranceId: 'dummy-word', detail: null }] };
      const state = { issues: [issue], pending: issue.applicationId ? [{ operationId: issue.applicationId, trackId: 'B', sourceInputId: word.id, sourceState: 'held', state: 'dispatched' }] : [], calls: [] as { issueId: string; action: string }[], planCalls: [] as string[], word };
      Object.assign(globalThis, { issueFixture: state });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: state.calls.length + state.planCalls.length + 1, dispatchPaused: true, tracks: [{ id: 'A', label: 'A', title: 'Dummy parent', status: 'done' }, { id: 'B', label: 'B', title: 'Dummy child', status: 'waiting' }], workers: kind === 'processes' ? [worker] : [], utterances: [word], plans: [plan], inheritanceIssues: state.issues, pendingInheritance: state.pending }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:inheritance-issue');
      ipcMain.handle('intercat:inheritance-issue', (_event, request) => { state.calls.push(request); if (request.action === 'keep') issue.kept = true; else state.issues.splice(0); });
      ipcMain.removeHandler('intercat:plan-command');
      ipcMain.handle('intercat:plan-command', (_event, request) => { state.planCalls.push(request.action.kind); return plan; });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    }, kind);
    const page = await app.firstWindow();
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const card = page.locator('[data-attention-card="inheritance-issue-00000000-0000-7000-8000-000000000012"]');
    await expect(card).toBeVisible();
    await expect(page.locator('[data-attention-card^="inheritance-"]')).toHaveCount(1);
    if (kind === 'processes') {
      const keep = card.getByRole('button', { name: 'Keep (A’s changes wait)', exact: true });
      await keep.click(); await expect(keep).toBeDisabled();
      await expect(card).toContainText('Kept running');
      const stop = card.getByRole('button', { name: 'Stop it', exact: true });
      await stop.focus();
      expect(await stop.evaluate(element => { const r = element.getBoundingClientRect(); return element.contains(document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2)); })).toBe(true);
      const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(info.outputPath('processes-kept.png'), Buffer.from(capture, 'base64'));
      await stop.press('Enter');
      await page.mouse.move(0, 0);
      await expect(card).toHaveCount(0);
      await expect(page.getByRole('button', { name: 'Finish file update', exact: true })).toBeVisible();
    } else {
      await expect(card).toContainText('src/dummy.ts');
      await expect(card.getByRole('button', { name: 'Stop it' })).toHaveCount(0);
      await card.getByRole('button', { name: 'Keep on hold', exact: true }).click();
      await expect(card.getByRole('button', { name: 'Keep on hold', exact: true })).toBeDisabled();
      await card.getByRole('button', { name: 'Review plan', exact: true }).click();
      const dialog = page.getByRole('dialog', { name: 'Review plan' });
      await expect(dialog).toContainText('Dummy held job');
      const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(info.outputPath('conflict-review.png'), Buffer.from(capture, 'base64'));
      await dialog.getByRole('button', { name: 'Close plan review' }).click();
    }
    const state = await app.evaluate(() => (globalThis as typeof globalThis & { issueFixture: { calls: { issueId: string; action: string }[]; planCalls: string[]; word: { state: string } } }).issueFixture);
    expect(state.word.state).toBe('held');
    expect(state.calls.map(call => call.action)).toEqual(kind === 'processes' ? ['keep', 'stop'] : ['keep']);
    expect(state.calls.every(call => call.issueId === '00000000-0000-7000-8000-000000000012')).toBe(true);
    expect(state.planCalls.every(action => action === 'review')).toBe(true);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
