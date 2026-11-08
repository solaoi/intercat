import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { DaemonStatus, HistoryRequest } from '../src/bridge';
import type { Plan, PlanCommand } from '../src/plans/types';

const planId = '00000000-0000-5000-8000-000000000021';
const parentId = '00000000-0000-5000-8000-000000000022';
const childId = '00000000-0000-5000-8000-000000000023';
for (const zoom of [1, 2]) for (const entry of ['deleted', 'dependency']) test(`Restore from ${entry} preserves a blocked history and its original redo at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-restore-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, { planId, parentId, childId, zoom }) => {
      const base = { folderId: 'dummy', constraints: [], doneWhen: 'Dummy file exists.', after: [], skipOrder: [], startRequested: false, utteranceId: null, detail: null };
      const plan: Plan = { id: planId, rev: 4, state: 'running', sourceId: 'source', sourceRevision: 1, startMode: 'manual', executeAllowed: true, lockedTrack: null,
        generation: { id: 'generation', state: 'ready', attempt: 0, detail: null }, basedOn: [], attachments: [], summary: 'Dummy restore history', assumptions: [], notices: [], seenRevision: null, rewrite: null,
        items: [
          { ...base, planItemId: parentId, trackRef: 'new:1', order: 0, title: 'Dummy parent', instruction: 'Create parent.txt.', state: 'cut' },
          { ...base, planItemId: childId, trackRef: 'new:2', order: 1, title: 'Dummy child', instruction: 'Create child.txt.', after: [{ id: parentId, inherit: false }], state: 'planned' },
          { ...base, planItemId: 'running-item', trackRef: 'C', order: 2, title: 'Running dummy', instruction: 'Keep running.', state: 'running' },
          { ...base, planItemId: 'other-cut', trackRef: 'new:3', order: 3, title: 'Other cut dummy', instruction: 'Create other.txt.', state: 'cut' },
        ] };
      const blocked = { entryId: 'move-c', label: 'Move plan item', text: 'Running dummy', blocked: 'can’t undo: Running dummy has started', at: 0 };
      const state: DaemonStatus = { connected: true, workerConfigured: true, sequence: 1, plans: [plan], workers: [], utterances: [], tracks: [{ id: 'C', label: 'C', title: 'Running dummy', status: 'working' }],
        editHistory: { undo: blocked, redo: null, takenBack: [] },
        planDependencies: { [parentId]: { id: parentId, planId, itemId: parentId, trackId: 'new:1', title: 'Dummy parent', state: 'cut', unclear: false, retryable: false, clipId: null, atMs: null } } };
      const fixture = { state, commands: [] as PlanCommand[], history: [] as HistoryRequest[], receipts: new Set<string>() };
      Object.assign(globalThis, { restoreFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command', (_event, request: PlanCommand) => {
        fixture.commands.push(request);
        if (request.action.kind === 'review') return structuredClone(plan);
        if (fixture.receipts.has(request.opId)) return structuredClone(plan);
        if (request.planId !== planId || request.revision !== plan.rev || request.action.kind !== 'restore' || request.action.itemId !== parentId) throw new Error('Wrong restore target');
        fixture.receipts.add(request.opId); plan.rev++; plan.state = 'held'; plan.items[0].state = 'planned'; state.sequence!++;
        state.planDependencies![parentId].state = 'planned';
        state.editHistory!.redo = { entryId: 'cut-a', label: 'Cut plan item', text: 'Dummy parent', blocked: null, at: 0 };
        throw new Error('Dummy restore acknowledgement lost');
      });
      ipcMain.removeHandler('intercat:history'); ipcMain.handle('intercat:history', (_event, request: HistoryRequest) => {
        fixture.history.push(request);
        if (request.action.kind !== 'redo' || request.action.entryId !== 'cut-a') throw new Error('Wrong history target');
        plan.items[0].state = 'cut'; plan.rev++; state.sequence!++; state.planDependencies![parentId].state = 'cut'; state.editHistory!.redo = null;
        return structuredClone(state);
      });
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
    }, { planId, parentId, childId, zoom });
    const page = await app.firstWindow(), draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill('Keep this independent restore draft.');
    const dialog = page.getByRole('dialog', { name: 'Review plan', exact: true });
    if (entry === 'deleted') {
      await page.getByRole('region', { name: 'Plans', exact: true }).getByRole('button', { name: 'Review plan', exact: true }).click();
      await dialog.getByText('Deleted plans · 2', { exact: true }).click();
      await dialog.locator('.deleted-plans > div').filter({ hasText: 'Dummy parent' }).getByRole('button', { name: 'Restore', exact: true }).click();
    } else {
      await page.getByRole('scrollbar', { name: 'All tracks', exact: true }).press('End');
      await page.locator(`[data-plan-item-id="${childId}"] .planned-item-select`).click();
      await page.getByRole('menuitem', { name: 'Restore Dummy parent', exact: true }).click();
    }
    await page.getByRole('button', { name: 'Retry plan action', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Retry plan action', exact: true })).toHaveCount(0);
    if (entry === 'deleted') {
      await expect(dialog.getByText('Deleted plans · 1', { exact: true })).toBeVisible();
      await expect(dialog.getByRole('heading', { name: 'Dummy parent', exact: true })).toBeAttached();
      await dialog.getByRole('button', { name: 'Back to plans', exact: true }).click();
    }
    const undo = page.getByRole('button', { name: 'Undo', exact: true });
    await undo.focus(); await undo.press('Meta+z');
    await expect(page.getByRole('alert').filter({ hasText: 'can’t undo: Running dummy has started' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Redo', exact: true })).toBeEnabled();
    await writeFile(info.outputPath('restore-history.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await page.getByRole('button', { name: 'Redo', exact: true }).click();
    const result = await app.evaluate(() => (globalThis as any).restoreFixture);
    const commands = result.commands.filter((request: PlanCommand) => request.action.kind === 'restore');
    expect(commands).toHaveLength(2); expect(commands[0]).toEqual(commands[1]);
    expect(result.history).toHaveLength(1); expect(result.history[0].action).toEqual({ kind: 'redo', entryId: 'cut-a' });
    expect(result.state.plans[0].state).toBe('held'); expect(result.state.plans[0].items[0].state).toBe('cut');
    expect(result.state.plans[0].items[2].state).toBe('running');
    await expect(draft).toHaveValue('Keep this independent restore draft.');
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
