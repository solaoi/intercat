import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { ApprovalResponse } from '../src/bridge';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

async function recoveredReplyFixture(app: ElectronApplication, mode: 'feedback' | 'restart') {
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }, mode) => {
    const workers = ['A', 'B'].map(trackId => ({ trackId, phase: 'approval', sessionId: 'session-' + trackId,
      activeTurn: 'turn-' + trackId, identity: null, detail: null, checkpointPending: false,
      checkpoint: null, prompts: [], output: null,
      approvals: [{ id: 'permission-' + trackId, requirement: { sourceIndex: 2 }, subject: 'Review the dummy command',
        reason: { kind: 'push', message: 'Review this step' }, canAllow: true, canDeny: true, replyMode: mode }] }));
    const state = { connected: true, sequence: 1, workerConfigured: true, workers,
      tracks: workers.map(worker => ({ id: worker.trackId, label: worker.trackId, title: 'Test work', status: 'waiting' })), utterances: [] };
    const fixture = { state, calls: [] as { opId: string; trackId: string; response: ApprovalResponse }[] };
    Object.assign(globalThis, { savedApprovalFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:worker-answer'); ipcMain.handle('intercat:worker-answer', (_event, request) => {
      fixture.calls.push(request);
      const worker = state.workers.find(worker => worker.trackId === request.trackId)!;
      worker.approvals = []; worker.phase = 'stopping'; state.sequence++;
      if (fixture.calls.length === 1) throw new Error('Lost reply acknowledgement');
    });
  }, mode);
  const page = await app.firstWindow();
  await page.reload();
  return page;
}

for (const mode of ['feedback', 'restart'] as const) {
  test(`a ${mode} reply survives a removed card and reload with its original operation`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-saved-approval-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      const page = await recoveredReplyFixture(app, mode);
      const ordinary = page.getByRole('textbox', { name: 'Request draft', exact: true });
      await ordinary.fill('Keep the ordinary request');
      const card = page.getByRole('group', { name: 'Approval for Worker A', exact: true });
      await card.getByRole('button', { name: 'Reply', exact: true }).click();
      await card.getByRole('textbox').fill('Keep this exact reply');
      await card.getByRole('button', { name: mode === 'feedback' ? 'Decline and reply' : 'Stop and send', exact: true }).click();
      const recovery = page.getByRole('region', { name: 'Saved approval replies' });
      await expect(recovery).toContainText('Keep this exact reply');
      await expect.poll(async () => page.evaluate(async () => {
        const entries = (await window.intercat.readDrafts('home')).book.entries;
        return Object.values(entries).find(entry => entry.context.kind === 'approval')?.pending?.kind;
      })).toBe('approval');
      await page.reload();
      await expect(recovery).toContainText('Keep this exact reply');
      await recovery.getByRole('button', { name: 'Copy to draft', exact: true }).click();
      await expect(ordinary).toHaveValue('Keep this exact reply');
      await expect(recovery).toContainText('Keep this exact reply');
      const saved = await page.evaluate(async () => (await window.intercat.readDrafts('home')).book);
      expect(saved.entries.work.text).toBe('Keep the ordinary request');
      await recovery.getByRole('button', { name: 'Retry response', exact: true }).click();
      await expect(recovery).toHaveCount(0);
      await expect(ordinary).toHaveValue('Keep this exact reply');
      const calls = await app.evaluate(() => (globalThis as typeof globalThis & { savedApprovalFixture: { calls: unknown[] } }).savedApprovalFixture.calls);
      expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
    } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
  });

  test(`unsent ${mode} replies retain independent owners and can return to the composer`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-unsent-approval-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      const page = await recoveredReplyFixture(app, mode);
      const ordinary = page.getByRole('textbox', { name: 'Request draft', exact: true });
      await ordinary.fill('The original request');
      for (const id of ['A', 'B']) {
        const card = page.getByRole('group', { name: 'Approval for Worker ' + id, exact: true });
        if (!await card.isVisible()) await page.getByRole('button', { name: /^\+\d+ more/ }).click();
        await card.getByRole('button', { name: 'Reply', exact: true }).click();
        await card.getByRole('textbox').fill('Reply for ' + id);
      }
      await app.evaluate(() => {
        const fixture = (globalThis as typeof globalThis & { savedApprovalFixture: { state: { workers: { trackId: string; phase: string; approvals: unknown[] }[]; sequence: number } } }).savedApprovalFixture;
        fixture.state.workers[0].phase = 'stopping'; fixture.state.workers[0].approvals = []; fixture.state.sequence++;
      });
      const recovery = page.getByRole('region', { name: 'Saved approval replies' });
      await expect(recovery).toContainText('Reply for A');
      await expect(recovery).not.toContainText('Reply for B');
      await recovery.getByRole('button', { name: 'Keep as draft', exact: true }).click();
      await expect(ordinary).toHaveValue('Reply for A');
      await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('A');
      await page.reload();
      await expect(ordinary).toHaveValue('Reply for A');
      const remainingReply = page.getByRole('textbox', { name: 'Reply to Worker B approval', exact: true });
      if (!await remainingReply.isVisible()) await page.getByRole('button', { name: /^\+\d+ more/ }).click();
      await expect(remainingReply).toHaveValue('Reply for B');
      const saved = await page.evaluate(async () => (await window.intercat.readDrafts('home')).book);
      expect(saved.entries.work.text).toBe('The original request');
      expect(saved.stack).toContain('work');
      await ordinary.focus();
      await page.mouse.move(10, 10);
      await app.evaluate((_, mode) => {
        const fixture = (globalThis as typeof globalThis & { savedApprovalFixture: { state: { workers: { phase: string; approvals: unknown[] }[]; sequence: number } } }).savedApprovalFixture;
        fixture.state.workers[0].phase = 'approval';
        fixture.state.workers[0].approvals = [{ id: 'permission-A', requirement: { sourceIndex: 2 }, subject: 'Review the dummy command', reason: { kind: 'push', message: 'Review this step' }, canAllow: true, canDeny: true, replyMode: mode }];
        fixture.state.sequence++;
      }, mode);
      const returned = page.getByRole('group', { name: 'Approval for Worker A', exact: true });
      await returned.getByRole('button', { name: 'Reply', exact: true }).click();
      await returned.getByRole('textbox').fill('A new reply for A');
      await expect(ordinary).toHaveValue('Reply for A');
      const calls = await app.evaluate(() => (globalThis as typeof globalThis & { savedApprovalFixture: { calls: unknown[] } }).savedApprovalFixture.calls);
      expect(calls).toHaveLength(0);
    } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
  });

  test(`draft storage failure ${mode === 'feedback' ? 'holds feedback' : 'preserves immediate stop'} and keeps the reply`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-failed-approval-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      const page = await recoveredReplyFixture(app, mode);
      const card = page.getByRole('group', { name: 'Approval for Worker A', exact: true });
      await card.getByRole('button', { name: 'Reply', exact: true }).click();
      await card.getByRole('textbox').fill('Preserve this reply through the failure');
      await app.evaluate(() => { (globalThis as typeof globalThis & { draftStorageFixture: { failWrites: boolean } }).draftStorageFixture.failWrites = true; });
      await card.getByRole('button', { name: mode === 'feedback' ? 'Decline and reply' : 'Stop and send', exact: true }).click();
      if (mode === 'feedback') {
        await expect(card.getByRole('alert')).toContainText('Drafts could not be saved');
        await expect(card.getByRole('textbox')).toHaveValue('Preserve this reply through the failure');
      } else {
        await expect(page.getByRole('region', { name: 'Saved approval replies' })).toContainText('Preserve this reply through the failure');
      }
      const count = await app.evaluate(() => (globalThis as typeof globalThis & { savedApprovalFixture: { calls: unknown[] } }).savedApprovalFixture.calls.length);
      expect(count).toBe(mode === 'feedback' ? 0 : 1);
      await app.evaluate(() => { (globalThis as typeof globalThis & { draftStorageFixture: { failWrites: boolean } }).draftStorageFixture.failWrites = false; });
      await page.getByRole('button', { name: 'Retry draft save', exact: true }).click();
      if (mode === 'feedback') {
        await card.getByRole('button', { name: 'Decline and reply', exact: true }).click();
        await expect(page.getByRole('region', { name: 'Saved approval replies' })).toBeVisible();
      }
      await page.getByRole('region', { name: 'Saved approval replies' }).getByRole('button', { name: 'Retry response', exact: true }).click();
      await expect(page.getByRole('region', { name: 'Saved approval replies' })).toHaveCount(0);
    } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
  });
}

for (const mode of ['feedback', 'restart'] as const) for (const zoom of [1, 2]) {
  test(`approval reply uses ${mode} at ${zoom * 100}% and retries its saved operation`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-approval-reply-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      await mockDrafts(app);
      await app.evaluate(({ ipcMain }, mode) => {
        const worker = { trackId: 'A', phase: 'approval', sessionId: 'session-A', activeTurn: 'turn-A', identity: null, detail: null,
          checkpointPending: false, checkpoint: null, prompts: [], output: null,
          approvals: [{ id: 'permission', requirement: { sourceIndex: 2 }, subject: 'Remove the temporary directory',
            reason: { kind: 'delete', message: 'Review this step' }, canAllow: true, canDeny: true, replyMode: mode }] };
        const state = { connected: true, sequence: 1, workerConfigured: true, workers: [worker],
          tracks: [{ id: 'A', label: 'A', title: 'Test work', status: 'waiting' }], utterances: [] };
        const fixture = { state, calls: [] as { opId: string; response: ApprovalResponse }[] };
        Object.assign(globalThis, { approvalReplyFixture: fixture });
        ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
        ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
        ipcMain.removeHandler('intercat:worker-answer'); ipcMain.handle('intercat:worker-answer', (_event, request) => {
          fixture.calls.push(request);
          if (fixture.calls.length === 1) throw new Error('The response could not be saved.');
          worker.approvals = []; worker.phase = mode === 'feedback' ? 'working' : 'stopping'; state.sequence++;
        });
      }, mode);
      const page = await app.firstWindow();
      await page.reload();
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(zoom); }, zoom);
      const card = page.getByRole('group', { name: 'Approval for Worker A', exact: true });
      const ordinaryDraft = page.getByRole('textbox', { name: 'Request draft', exact: true });
      await ordinaryDraft.fill('An ordinary request stays separate.');
      await card.getByRole('button', { name: 'Reply', exact: true }).click();
      const input = card.getByRole('textbox', { name: 'Reply to Worker A approval', exact: true });
      const submit = card.getByRole('button', { name: mode === 'feedback' ? 'Decline and reply' : 'Stop and send', exact: true });
      await expect(submit).toBeDisabled();
      await expect(card.getByText(mode === 'feedback' ? 'declines this step and sends your words' : 'sends after stopping the current turn', { exact: true })).toBeVisible();
      const text = 'はい、実行して。ただし元のフォルダは残してください。';
      await input.fill(text);
      await page.keyboard.press('Escape');
      await expect(card.getByRole('button', { name: 'Reply', exact: true })).toBeFocused();
      await expect(ordinaryDraft).toHaveValue('An ordinary request stays separate.');
      await card.getByRole('button', { name: 'Reply', exact: true }).click();
      await expect(input).toHaveValue(text);
      await submit.scrollIntoViewIfNeeded();
      await expect(submit).toBeInViewport({ ratio: 1 });
      await expect(input).toBeInViewport({ ratio: 1 });
      await submit.click();
      await expect(card.getByRole('alert')).toContainText('The response could not be saved.');
      await expect(input).toHaveValue(text);
      await submit.scrollIntoViewIfNeeded();
      await expect(input).toBeInViewport({ ratio: 1 });
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath('reply.png'), Buffer.from(pixels, 'base64'));
      await submit.click();
      await expect(card).toHaveCount(0);
      const calls = await app.evaluate(() => (globalThis as typeof globalThis & { approvalReplyFixture: { calls: { opId: string; response: ApprovalResponse }[] } }).approvalReplyFixture.calls);
      expect(calls).toHaveLength(2);
      expect(calls[0]).toEqual(calls[1]);
      expect(calls[1].response).toEqual({ action: 'reply', text, mode });
      await writeFile(test.info().outputPath('responses.json'), JSON.stringify(calls, null, 2));
    } finally {
      await closeWindow(app);
      await rm(directory, { recursive: true, force: true });
    }
  });
}
