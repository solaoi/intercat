import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { Plan, PlanCommand } from '../src/plans/types';

const plan = (): Plan => ({
  id: '00000000-0000-5000-8000-000000000001', rev: 2, state: 'draft', sourceId: 'source', sourceRevision: 1, startMode: 'manual',
  executeAllowed: false, lockedTrack: null, generation: { id: 'generation', state: 'ready', attempt: 0, detail: null },
  basedOn: [], attachments: [], summary: 'Prepare a dummy page and its checks.', assumptions: [], notices: [], seenRevision: null, rewrite: null,
  items: ['Page', 'Checks', 'Notes'].map((title, index) => ({ planItemId: `item-${index}`, trackRef: `new:${index + 1}`, folderId: 'folder', order: index,
    title, instruction: `Prepare the dummy ${title.toLowerCase()}.`, constraints: ['Use only dummy data.'], doneWhen: `The ${title.toLowerCase()} can be checked.`,
    after: index === 1 ? [{ id: 'item-0', inherit: false }] : [], state: 'planned', skipOrder:[],startRequested:false,utteranceId: null, detail: null })),
});

for (const zoom of [1, 2]) test(`Plan review keeps the displayed revision and recovers a stale Start at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-review-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, initial: Plan) => {
      const state = { plan: initial, calls: [] as PlanCommand[] };
      Object.assign(globalThis, { planFixture: state });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: state.plan.rev, tracks: [], workers: [], utterances: [{ id: 'source', text: 'Prepare a dummy page and its checks.', revision: 1, state: 'landed', target: { mode: 'new' }, createdAt: 0, attachments: [] }], plans: [state.plan] }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:plan-command');
      ipcMain.handle('intercat:plan-command', (_event, request: PlanCommand) => {
        state.calls.push(request);
        if (request.revision !== state.plan.rev) throw new Error('The plan changed · review again');
        if (request.action.kind === 'review') state.plan.seenRevision = state.plan.rev;
        if (request.action.kind === 'start') { state.plan.state = 'running'; state.plan.rev += 1; state.plan.items[0].state = 'running'; }
        if (request.action.kind === 'hold') { state.plan.state = 'held'; state.plan.rev += 1; }
        if (request.action.kind === 'cut' || request.action.kind === 'restore') {
          const itemId = request.action.itemId;
          const item = state.plan.items.find(item => item.planItemId === itemId)!;
          item.state = request.action.kind === 'cut' ? 'cut' : 'planned'; state.plan.rev += 1;
        }
        return structuredClone(state.plan);
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    }, plan());
    const page = await app.firstWindow();
    await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
    const strip = page.getByRole('region', { name: 'Plans', exact: true });
    await expect(strip).toContainText('waiting for your go');
    if (zoom === 1) {
      await page.getByRole('button', { name: 'Page · waiting for your go', exact: true }).click();
      await expect(page.getByRole('heading', { name: 'Page', exact: true })).toBeFocused();
      await page.getByRole('button', { name: 'Close plan review' }).click();
    }
    await strip.getByRole('button', { name: 'Review plan', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Review plan' });
    await expect(dialog.locator('.plan-source')).toHaveText('Your request: Prepare a dummy page and its checks.');
    await expect(dialog).toContainText('Prepare the dummy page.');
    await expect(dialog).toContainText('Use only dummy data.');
    await expect(dialog).toContainText('The checks can be checked.');
    await expect(dialog).toContainText('order only');
    await app.evaluate(() => {
      const fixture = (globalThis as typeof globalThis & { planFixture: { plan: Plan } }).planFixture;
      fixture.plan.rev += 1; fixture.plan.items[0].instruction = 'Use the revised dummy page instructions.';
    });
    await expect(dialog.getByText('The plan changed · review again', { exact: false })).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Start plan · 3 tracks' })).toHaveCount(0);
    await dialog.getByRole('button', { name: 'Review current plan' }).click();
    await expect(dialog).toContainText('Use the revised dummy page instructions.');
    await dialog.getByRole('button', { name: 'Back to plans', exact: true }).click();
    await expect(strip.getByRole('button', { name: 'Start plan · 3 tracks' })).toBeEnabled();
    await strip.getByRole('button', { name: 'Start plan · 3 tracks' }).click();
    await expect(strip.getByRole('button', { name: 'Hold the rest' })).toBeEnabled();
    await strip.getByRole('button', { name: 'Hold the rest' }).click();
    const state = await app.evaluate(() => (globalThis as typeof globalThis & { planFixture: { plan: Plan; calls: PlanCommand[] } }).planFixture);
    expect(state.plan.state).toBe('held');
    expect(state.plan.items[0].state).toBe('running');
    expect(state.calls.filter(call => call.action.kind === 'start').map(call => call.revision)).toEqual([3]);
    await strip.getByRole('button', { name: 'Start plan · 3 tracks' }).click();
    const holdRest = strip.getByRole('button', { name: 'Hold the rest', exact: true });
    await expect(holdRest).toBeEnabled();
    await holdRest.focus();
    await holdRest.press('Enter');
    await expect(strip).toContainText('held');
    const held = await app.evaluate(() => (globalThis as typeof globalThis & { planFixture: { plan: Plan } }).planFixture.plan);
    expect(held.items[0].state).toBe('running');
    expect(held.items[1].state).toBe('planned');
    await strip.getByRole('button', { name: 'Review plan', exact: true }).click();
    await dialog.getByRole('button', { name: 'Cut Notes', exact: true }).click();
    await dialog.getByText('Deleted plans · 1', { exact: true }).click();
    await expect(dialog.locator('.deleted-plans')).toContainText('Notes');
    await dialog.locator('.deleted-plans').getByRole('button', { name: 'Restore', exact: true }).click();
    await expect(dialog.getByRole('heading', { name: 'Notes', exact: true })).toBeAttached();
    await expect(dialog.locator('footer')).toContainText('held');
    const rect = await dialog.evaluate(element => { const rect = element.getBoundingClientRect(); return { left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom, width: innerWidth, height: innerHeight }; });
    expect(rect.left).toBeGreaterThanOrEqual(0); expect(rect.right).toBeLessThanOrEqual(rect.width);
    expect(rect.top).toBeGreaterThanOrEqual(0); expect(rect.bottom).toBeLessThanOrEqual(rect.height);
    const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('plan-review.png'), Buffer.from(capture, 'base64'));
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});

test('Plan only keeps its false execution flag through a lost response and has a keyboard shortcut', async () => {
  const directory = await mkdtemp('/private/tmp/ic-plan-only-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain }) => {
      const requests: { opId: string; text: string; executeAllowed?: boolean }[] = [];
      Object.assign(globalThis, { planOnlyRequests: requests });
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, tracks: [], workers: [], utterances: [] }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:stage');
      ipcMain.handle('intercat:stage', (_event, request) => {
        requests.push(request);
        if (requests.length === 1) throw new Error('Saved reply lost');
        return { id: request.opId, text: request.text, attachments: [], state: 'landed', revision: 1, target: request.target, createdAt: 0, decision: { kind: 'plan' }, executeAllowed: request.executeAllowed };
      });
    });
    const page = await app.firstWindow();
    const draft = page.getByRole('textbox', { name: 'Request draft' });
    await draft.fill('Plan a dummy badge.');
    await page.getByRole('button', { name: 'More send options' }).click();
    await page.getByRole('menuitem', { name: 'Plan only' }).click();
    await expect(page.getByRole('button', { name: 'Retry save' })).toBeEnabled();
    await page.getByRole('button', { name: 'Retry save' }).click();
    await expect(draft).toHaveValue('');
    await draft.fill('Plan a dummy guide.');
    await draft.press('Meta+Shift+p');
    await expect(draft).toHaveValue('');
    const requests = await app.evaluate(() => (globalThis as typeof globalThis & { planOnlyRequests: { opId: string; executeAllowed: boolean }[] }).planOnlyRequests);
    expect(requests).toHaveLength(3); expect(requests[0]).toEqual(requests[1]); expect(requests.every(request => request.executeAllowed === false)).toBe(true);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
