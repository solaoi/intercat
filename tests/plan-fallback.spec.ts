import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { Plan, PlanCommand } from '../src/plans/types';

for (const zoom of [1, 2]) test(`A timed-out plan offers retry and one job without losing the saved choice at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-fallback-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const plan: Plan = { id: 'dummy-plan', rev: 1, state: 'draft', sourceId: 'dummy-source', sourceRevision: 1, startMode: 'manual',
        executeAllowed: true, lockedTrack: null, generation: { id: 'dummy-generation', state: 'timed-out', attempt: 0, detail: 'The Planner did not finish within 60 seconds.' },
        basedOn: [], attachments: [], summary: 'Plan the dummy badge.', assumptions: [], notices: [], items: [], seenRevision: null, rewrite: null };
      const state = { plan, calls: [] as PlanCommand[], receipts: new Set<string>(), loseReply: true };
      Object.assign(globalThis, { fallbackFixture: state });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: plan.rev, tracks: [], workers: [], utterances: [], plans: [plan] }));
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command', (_event, request: PlanCommand) => {
        state.calls.push(request);
        if (state.receipts.has(request.opId)) return structuredClone(plan);
        if (request.revision !== plan.rev) throw new Error('The plan changed · review again');
        if (request.action.kind === 'retry') { plan.sourceRevision++; plan.rev++; plan.generation.state = 'running'; }
        if (request.action.kind === 'run-as-one') {
          plan.sourceRevision++; plan.rev++; plan.state = 'single-job'; plan.generation.state = 'ready'; plan.generation.detail = null;
          state.receipts.add(request.opId);
          if (state.loseReply) { state.loseReply = false; throw new Error('Saved choice reply lost'); }
        }
        return structuredClone(plan);
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
      BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow();
    const plans = page.getByRole('region', { name: 'Plans', exact: true });
    const oneJob = plans.getByRole('button', { name: 'Run as one job', exact: true });
    await expect(oneJob).toBeEnabled();
    for (const name of ['Retry planning', 'Run as one job']) {
      const button = plans.getByRole('button', { name, exact: true });
      await expect(button).toBeInViewport();
      expect(await button.evaluate(element => getComputedStyle(element).backgroundColor)).toBe('rgba(0, 0, 0, 0)');
    }
    await writeFile(info.outputPath('plan-timeout.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await plans.getByRole('button', { name: 'Retry planning', exact: true }).click();
    await expect(plans).toContainText('planning');
    await expect(oneJob).toHaveCount(0);
    await app.evaluate(() => { const { plan } = (globalThis as typeof globalThis & { fallbackFixture: { plan: Plan } }).fallbackFixture; plan.generation.state = 'timed-out'; });
    await expect(oneJob).toBeEnabled();
    await oneJob.click();
    await expect(plans).toContainText('sent as one job');
    await page.getByRole('button', { name: 'Retry plan action', exact: true }).click();
    await expect(page.getByText('Saved choice reply lost', { exact: true })).toHaveCount(0);
    const state = await app.evaluate(() => (globalThis as typeof globalThis & { fallbackFixture: { plan: Plan; calls: PlanCommand[] } }).fallbackFixture);
    const calls = state.calls.filter(call => call.action.kind === 'run-as-one');
    expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
    expect(state.plan.sourceRevision).toBe(3);
    await app.evaluate(() => { const { plan } = (globalThis as typeof globalThis & { fallbackFixture: { plan: Plan } }).fallbackFixture; plan.state = 'draft'; plan.executeAllowed = false; plan.generation.state = 'timed-out'; plan.rev++; });
    await expect(plans.getByRole('button', { name: 'Retry planning', exact: true })).toBeEnabled();
    await expect(oneJob).toHaveCount(0);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
