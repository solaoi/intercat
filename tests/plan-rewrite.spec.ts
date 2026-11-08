import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { Plan, PlanRewriteAction } from '../src/plans/types';

const initial = (): Plan => ({
  id: '00000000-0000-5000-8000-000000000001', rev: 2, state: 'running', sourceId: 'source', sourceRevision: 1, startMode: 'manual',
  executeAllowed: true, lockedTrack: null, generation: { id: 'generation', state: 'ready', attempt: 0, detail: null },
  basedOn: [], attachments: [], summary: 'Build two dummy objects.', assumptions: [], notices: [], seenRevision: null, rewrite: null,
  items: ['Badge', 'Guide'].map((title, index) => ({ planItemId: `00000000-0000-5000-8000-00000000000${index + 2}`, trackRef: `worker-${index}`, folderId: 'dummy-folder', order: index,
    title, instruction: `Build the dummy ${title.toLowerCase()}.`, constraints: [], doneWhen: 'The dummy object exists.', after: [], state: index ? 'running' : 'planned', skipOrder:[],startRequested:false,utteranceId: null, detail: null })),
});
for (const zoom of [1, 2]) test(`A held item is revised from words and accepted separately at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-rewrite-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, plan: Plan) => {
      const fixture = { plan, calls: [] as PlanRewriteAction[], receipts: new Map<string, Plan>(), failSubmit: true, failApply: true };
      Object.assign(globalThis, { rewriteFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: fixture.plan.rev, tracks: [], workers: [], utterances: [], plans: [fixture.plan] }));
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command', () => structuredClone(fixture.plan));
      ipcMain.removeHandler('intercat:plan-edit'); ipcMain.handle('intercat:plan-edit', (_event, action: PlanRewriteAction) => {
        fixture.calls.push(action);
        if ('opId' in action && fixture.receipts.has(action.opId)) return structuredClone(fixture.plan);
        if (action.kind === 'begin') {
          const before = structuredClone(fixture.plan.items[0]); fixture.plan.items[0].state = 'editing'; fixture.plan.rev++;
          fixture.plan.rewrite = { owner: 'dummy-owner', token: action.token, itemId: before.planItemId, revision: fixture.plan.rev, renewed: { epochMs: 0, monoMs: 0, bootId: 'dummy', timelineMs: 0 }, before, words: null, proposal: null };
        } else if (action.kind === 'submit') {
          if (!fixture.plan.rewrite) throw new Error('Editing was interrupted. Reopen this item.');
          fixture.plan.rewrite!.words = action.text; fixture.plan.generation.state = 'running'; fixture.plan.rev++;
          fixture.receipts.set(action.opId, structuredClone(fixture.plan));
          if (fixture.failSubmit) { fixture.failSubmit = false; throw new Error('Saved revision reply lost'); }
        } else if (action.kind === 'apply') {
          if (fixture.failApply) { fixture.failApply = false; throw new Error('Save unavailable'); }
          const proposal = fixture.plan.rewrite!.proposal!;
          Object.assign(fixture.plan.items[0], proposal, { state: 'planned' }); fixture.plan.rewrite = null; fixture.plan.rev++;
          fixture.receipts.set(action.opId, structuredClone(fixture.plan));
        } else if (action.kind === 'cancel') {
          fixture.plan.items[0].state = 'planned'; fixture.plan.rewrite = null; fixture.plan.rev++;
        }
        return structuredClone(fixture.plan);
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    }, initial());
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const page = await app.firstWindow();
    const draft = page.getByRole('textbox', { name: 'Request draft' });
    await draft.fill('Unrelated draft stays here.');
    await page.getByRole('button', { name: 'Review plan', exact: true }).click();
    await page.getByRole('button', { name: 'Actions for Badge', exact: true }).click();
    await page.getByRole('menuitem', { name: 'Revise instructions…', exact: true }).click();
    await expect(page.getByLabel('Plan item being revised')).toContainText('Badge');
    await expect(draft).toHaveValue(''); await expect(draft).toBeFocused();
    await draft.fill('Use a blue robot badge instead.');
    if (zoom === 2) {
      await app.evaluate(() => { const { plan } = (globalThis as typeof globalThis & { rewriteFixture: { plan: Plan } }).rewriteFixture; plan.rewrite = null; plan.items[0].state = 'planned'; plan.state = 'held'; plan.rev++; });
      await page.getByRole('button', { name: 'Revise item', exact: false }).click();
      await page.getByRole('button', { name: 'Reopen item for editing', exact: true }).click();
      await expect(draft).toHaveValue('Use a blue robot badge instead.');
    }
    await page.getByRole('button', { name: 'Revise item', exact: false }).click();
    await expect(page.getByRole('button', { name: 'Retry save', exact: false })).toBeEnabled();
    await page.getByRole('button', { name: 'Retry save', exact: false }).click();
    const revision = page.getByRole('region', { name: 'Item revision' });
    await expect(revision).toContainText('Revising item…');
    await app.evaluate(() => {
      const { plan } = (globalThis as typeof globalThis & { rewriteFixture: { plan: Plan } }).rewriteFixture;
      plan.generation.state = 'ready'; plan.rewrite!.proposal = { id: plan.items[0].planItemId, trackRef: plan.items[0].trackRef, folderId: plan.items[0].folderId, title: 'Blue robot badge', instruction: 'Build a blue robot badge.', constraints: ['Only dummy data.'], doneWhen: 'The blue robot badge exists.', after: [] };
    });
    await expect(revision).toContainText('Build a blue robot badge.');
    let state = await app.evaluate(() => (globalThis as typeof globalThis & { rewriteFixture: { plan: Plan; calls: PlanRewriteAction[] } }).rewriteFixture);
    expect(state.plan.items[0].instruction).toBe('Build the dummy badge.'); expect(state.plan.items[1].state).toBe('running');
    const submits = state.calls.filter(call => call.kind === 'submit'); expect(submits).toHaveLength(zoom === 2 ? 3 : 2); expect(submits.at(-2)).toEqual(submits.at(-1));
    await writeFile(info.outputPath('item-revision.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await revision.getByRole('button', { name: 'Use revised item', exact: true }).click();
    await expect(revision.getByRole('button', { name: 'Retry revision action', exact: true })).toBeEnabled();
    await revision.getByRole('button', { name: 'Retry revision action', exact: true }).click();
    await expect(revision).toHaveCount(0);
    state = await app.evaluate(() => (globalThis as typeof globalThis & { rewriteFixture: { plan: Plan; calls: PlanRewriteAction[] } }).rewriteFixture);
    expect(state.plan.items[0].instruction).toBe('Build a blue robot badge.'); expect(state.plan.items[1].state).toBe('running');
    const applies = state.calls.filter(call => call.kind === 'apply'); expect(applies).toHaveLength(2); expect(applies[0]).toEqual(applies[1]);
    await page.getByRole('button', { name: 'Close plan review', exact: true }).click();
    await expect(draft).toHaveValue('Unrelated draft stays here.');
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
