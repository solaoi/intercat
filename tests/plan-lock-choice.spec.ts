import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { DaemonStatus } from '../src/bridge';
import type { Plan, PlanCommand } from '../src/plans/types';

for (const zoom of [1, 2]) for (const unlock of [false, true]) test(`Locked plan choice ${unlock ? 'unlocks' : 'keeps A'} at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-lock-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const plan: Plan = { id: '00000000-0000-5000-8000-000000000041', rev: 3, state: 'draft', sourceId: 'source', sourceRevision: 1,
        executeAllowed: true, lockedTrack: 'A', startMode: 'manual', generation: { id: 'attempt', state: 'unlock-required', attempt: 1, detail: 'Keep every item on the locked track.\n'.repeat(8) },
        basedOn: [], attachments: [], summary: '', assumptions: [], notices: [], items: [], seenRevision: null, rewrite: null };
      const state: DaemonStatus = { connected: true, sequence: 1, plans: [plan], workers: [], utterances: [], tracks: [{ id: 'A', label: 'A', title: 'Dummy worker A', status: 'done' }, { id: 'B', label: 'B', title: 'Dummy worker B', status: 'done' }] };
      const fixture = { state, calls: [] as PlanCommand[], receipts: new Set<string>() };
      Object.assign(globalThis, { planLockFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command', (_event, request: PlanCommand) => {
        fixture.calls.push(request);
        if (fixture.receipts.has(request.opId)) return structuredClone(plan);
        if (request.planId !== plan.id || request.revision !== plan.rev) throw new Error('The plan changed · review again');
        if (request.action.kind === 'review') { plan.seenRevision = plan.rev; return structuredClone(plan); }
        if (request.action.kind !== 'resolve-lock' || plan.generation.state !== 'unlock-required') throw new Error('Wrong lock choice');
        fixture.receipts.add(request.opId); plan.rev++; plan.sourceRevision++; plan.lockedTrack = request.action.unlock ? null : 'A';
        plan.generation = { id: 'next-attempt', state: 'ready', attempt: 0, detail: null };
        if (request.action.unlock) {
          plan.items = [{ planItemId: 'split-item', trackRef: 'new:1', folderId: 'dummy', order: 0, title: 'Dummy task', instruction: 'Dummy only', constraints: [], doneWhen: 'Done', after: [], state: 'planned', skipOrder: [], startRequested: false, utteranceId: null, detail: null }];
        } else {
          plan.state = 'single-job';
          state.utterances!.push({ id: 'source', revision: 2, text: 'Dummy original request', target: { mode: 'lock', trackId: 'A' }, state: 'staged', attachments: [], createdAt: 0, reason: null, decision: null });
        }
        state.sequence!++;
        throw new Error('Dummy saved choice acknowledgement lost');
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760); BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow(), strip = page.getByRole('region', { name: 'Plans', exact: true });
    const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill('Keep this separate dummy request.'); await page.getByRole('combobox', { name: 'Request destination' }).selectOption('B');
    await expect(strip.getByRole('group', { name: 'Unlock and split?' })).toBeVisible();
    await expect(strip.getByRole('button', { name: 'Run as one job', exact: true })).toHaveCount(0);
    await strip.getByRole('button', { name: 'Review plan', exact: true }).click();
    const detail = page.getByRole('dialog', { name: 'Review plan', exact: true });
    await expect(detail).toContainText('Keep every item on the locked track.');
    const choices = detail.getByRole('group', { name: 'Unlock and split?', exact: true });
    for (const name of ['Yes', 'Keep on A']) { await expect(choices.getByRole('button', { name, exact: true })).toBeInViewport(); }
    await app.evaluate(() => { const fixture = (globalThis as any).planLockFixture; fixture.state.plans[0].rev++; fixture.state.sequence++; });
    await expect(choices.getByRole('button', { name: 'Yes', exact: true })).toBeDisabled();
    await expect(choices.getByRole('button', { name: 'Keep on A', exact: true })).toBeDisabled();
    await detail.getByRole('button', { name: 'Review current plan', exact: true }).click();
    await expect(choices.getByRole('button', { name: 'Yes', exact: true })).toBeEnabled();
    await writeFile(info.outputPath('lock-choice.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    if (unlock) {
      await app.evaluate(() => { const fixture = (globalThis as any).planLockFixture; fixture.state.plans[0].executeAllowed = false; fixture.state.sequence++; });
      await expect(choices.getByRole('button', { name: 'Keep on A', exact: true })).toBeDisabled();
      await expect(choices.getByRole('button', { name: 'Yes', exact: true })).toBeEnabled();
    }
    await choices.getByRole('button', { name: unlock ? 'Yes' : 'Keep on A', exact: true }).click();
    await detail.getByRole('button', { name: 'Retry plan action', exact: true }).click();
    await expect(detail.getByRole('button', { name: 'Retry plan action', exact: true })).toHaveCount(0);
    await expect(detail).toContainText(unlock ? 'waiting for your go' : 'sent as one job');
    await detail.getByRole('button', { name: 'Close plan review', exact: true }).click();
    await expect(draft).toHaveValue('Keep this separate dummy request.'); await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
    const result = await app.evaluate(() => (globalThis as any).planLockFixture);
    const calls = result.calls.filter((call: PlanCommand) => call.action.kind === 'resolve-lock');
    expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]); expect(calls[0].action).toEqual({ kind: 'resolve-lock', unlock });
    expect(result.state.plans[0].lockedTrack).toBe(unlock ? null : 'A'); expect(result.state.plans[0].sourceRevision).toBe(2);
    expect(result.state.workers).toEqual([]); expect(result.state.utterances).toHaveLength(unlock ? 0 : 1);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
