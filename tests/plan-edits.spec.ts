import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { Plan, PlanCommand, PlanRewriteAction } from '../src/plans/types';
import type { HistoryRequest } from '../src/bridge';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { requireVoiceOver } from './support/voiceover';

const plan = (): Plan => ({
  id: 'dummy-plan', rev: 1, state: 'draft', sourceId: 'dummy-source', sourceRevision: 1, startMode: 'manual',
  executeAllowed: false, lockedTrack: null, generation: { id: 'dummy-generation', state: 'ready', attempt: 0, detail: null },
  basedOn: [], attachments: [], summary: 'Prepare a dummy badge and guide.', assumptions: [], notices: [], seenRevision: null, rewrite: null,
  items: ['Draft badge', 'Verify badge', 'Write guide'].map((title, index) => ({
    planItemId: `dummy-item-${index}`, trackRef: index === 2 ? 'worker-b' : 'worker-a', folderId: 'dummy-folder', order: index,
    title, instruction: `Do only the dummy task: ${title}.`, constraints: [], doneWhen: 'The dummy file is saved.', after: [], state: 'planned', skipOrder:[],startRequested:false,utteranceId: null, detail: null,
  })),
});

async function preparePlan(app: ElectronApplication, zoom: number) {
  await app.evaluate(({ ipcMain, BrowserWindow }, initial: Plan) => {
    const state = { plan: initial, calls: [] as PlanCommand[], reject: false, before: [] as Plan[], restored: false, histories: [] as HistoryRequest[], edits: [] as PlanRewriteAction[], dependencyReceipts: new Set<string>(), failDependency: true };
    Object.assign(globalThis, { editPlanFixture: state });
    const snapshot = () => ({ connected: true, sequence: state.plan.rev,
      tracks: [{ id: 'worker-a', label: 'A', title: 'Dummy badge', status: 'planned' }, { id: 'worker-b', label: 'B', title: 'Dummy guide', status: 'planned' }],
      workers: [], workspaces: [], utterances: [], plans: [state.plan],
      editHistory: { undo: state.before.length ? { entryId: 'dummy-edit', label: 'Move plan item', text: 'Draft badge', at: 0, blocked: null } : null, redo: null, takenBack: state.restored ? [] : [{ id: 'dummy-taken-back', text: 'Dummy words kept while editing the plan', revision: 2 }] },
    });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', snapshot);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command', (_event, request: PlanCommand) => {
      state.calls.push(request);
      if (state.reject) throw new Error('This move would create a dependency cycle.');
      if (request.revision !== state.plan.rev) throw new Error('The plan changed · review again');
      const action = request.action;
      if (action.kind === 'review') { state.plan.seenRevision = state.plan.rev; return structuredClone(state.plan); }
      state.before.push(structuredClone(state.plan));
      if ('itemId' in action) {
        const item = state.plan.items.find(item => item.planItemId === action.itemId)!;
        if (action.kind === 'move') {
          const items = state.plan.items.filter(other => other.trackRef === item.trackRef).sort((a, b) => a.order - b.order);
          const index = items.indexOf(item); const other = items[index + (action.direction === 'earlier' ? -1 : 1)];
          if (other) [other.order, item.order] = [item.order, other.order];
        } else if (action.kind === 'move-to') { item.trackRef = action.trackId; item.order = Math.max(...state.plan.items.map(item => item.order)) + 1; }
        else if (action.kind === 'cut') item.state = 'cut';
      }
      state.plan.rev += 1; state.plan.seenRevision = null; return structuredClone(state.plan);
    });
    ipcMain.removeHandler('intercat:plan-edit'); ipcMain.handle('intercat:plan-edit', (_event, action: PlanRewriteAction) => {
      state.edits.push(action);
      if ('opId' in action && state.dependencyReceipts.has(action.opId)) return structuredClone(state.plan);
      if (action.kind === 'begin') {
        const before = structuredClone(state.plan.items.find(item => item.planItemId === action.itemId)!);
        state.plan.items.find(item => item.planItemId === action.itemId)!.state = 'editing'; state.plan.rev++;
        state.plan.rewrite = { owner: 'dummy-owner', token: action.token, itemId: action.itemId, revision: state.plan.rev, renewed: { epochMs: 0, monoMs: 0, bootId: 'dummy', timelineMs: 0 }, before, words: null, proposal: null };
      } else if (action.kind === 'dependencies') {
        const before = structuredClone(state.plan); before.items.find(item => item.planItemId === state.plan.rewrite!.itemId)!.state = 'planned'; before.rewrite = null; state.before.push(before);
        const item = state.plan.items.find(item => item.planItemId === state.plan.rewrite!.itemId)!; item.after = action.after; item.state = 'planned'; state.plan.rewrite = null; state.plan.rev++;
        state.dependencyReceipts.add(action.opId);
        if (state.failDependency) { state.failDependency = false; throw new Error('Saved dependency reply lost'); }
      } else if (action.kind === 'cancel') { state.plan.rewrite = null; state.plan.rev++; }
      return structuredClone(state.plan);
    });
    ipcMain.removeHandler('intercat:history'); ipcMain.handle('intercat:history', (_event, request: HistoryRequest) => { state.histories.push(request); if (request.action.kind === 'restore') state.restored = true; else { const restored = state.before.pop(); if (restored) { restored.rev = state.plan.rev + 1; state.plan = restored; } } return snapshot(); });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, plan());
  await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
}

const nativeRun = promisify(execFile);
async function nativePlanKey(app: ElectronApplication, keyCode: number, modifier?: 'control' | 'option') {
  expect(await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].isFocused())).toBe(true);
  await nativeRun('/usr/bin/osascript', ['-e', `tell application "System Events" to key code ${keyCode}${modifier ? ` using ${modifier} down` : ''}`], { timeout: 2500 });
}

async function spokenPlan(app: ElectronApplication, expected: RegExp) {
  let phrase = '';
  try {
    await expect.poll(async () => {
      expect(await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].isFocused())).toBe(true);
      phrase = (await nativeRun('/usr/bin/osascript', ['-e', 'tell application "VoiceOver" to get content of last phrase'], { timeout: 2500 })).stdout.trim();
      return expected.test(phrase);
    }, { timeout: 6000, intervals: [50, 100, 200] }).toBe(true);
  } catch (cause) {
    throw new Error(`Expected VoiceOver ${expected}; last phrase: ${phrase}`, { cause });
  }
  return phrase;
}

for (const zoom of [1, 2]) test(`Native VoiceOver announces plan moves and their rejection at ${zoom * 100}%`, async ({}, info) => {
  test.skip(process.env.INTERCAT_NATIVE_PLAN_VOICEOVER !== '1', 'Requires the unlocked Mac and VoiceOver scripting enabled by the user.');
  await requireVoiceOver();
  const directory = await mkdtemp('/private/tmp/ic-plan-voiceover-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  const phrases: { action: string; phrase: string }[] = [];
  await mockDrafts(app);
  try {
    await preparePlan(app, zoom);
    await app.evaluate(({ BrowserWindow, app }) => { const win = BrowserWindow.getAllWindows()[0]; win.show(); app.focus({ steal: true }); win.focus(); });
    const page = await app.firstWindow();
    const tile = () => page.getByRole('button', { name: 'Draft badge · waiting for your go', exact: true });
    await tile().focus();
    await nativePlanKey(app, 36, 'control');
    await expect(page.getByRole('menu', { name: 'Plan item actions for Draft badge', exact: true })).toBeVisible();
    phrases.push({ action: 'open menu', phrase: await spokenPlan(app, /Move earlier|Plan item actions/) });
    await nativePlanKey(app, 53);
    await expect(page.getByRole('menu', { name: 'Plan item actions for Draft badge', exact: true })).toHaveCount(0);
    await expect(tile()).toBeFocused();
    await nativePlanKey(app, 124, 'option');
    phrases.push({ action: 'move later', phrase: await spokenPlan(app, /Moved to A, after Verify badge/) });
    await nativePlanKey(app, 123, 'option');
    phrases.push({ action: 'move earlier', phrase: await spokenPlan(app, /Moved to A, first item/) });
    await nativePlanKey(app, 125, 'option');
    phrases.push({ action: 'move to B', phrase: await spokenPlan(app, /Moved to B, after Write guide/) });
    await app.evaluate(() => { (globalThis as any).editPlanFixture.reject = true; });
    await nativePlanKey(app, 123, 'option');
    const rejection = await spokenPlan(app, /This move would create a dependency cycle/);
    expect(rejection).not.toContain('Error invoking remote method');
    phrases.push({ action: 'reject cycle', phrase: rejection });
    await writeFile(info.outputPath('plan-voiceover.json'), JSON.stringify({ zoom, phrases }, null, 2));
    const capture = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('plan-voiceover.png'), Buffer.from(capture, 'base64'));
  } finally {
    await writeFile(info.outputPath('plan-voiceover-progress.json'), JSON.stringify({ zoom, phrases }, null, 2));
    await closeWindow(app); await rm(directory, { recursive: true, force: true });
  }
});

for (const zoom of [1, 2]) test(`Plan moves keep keyboard selection and recover an unconfirmed change at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-edits-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await preparePlan(app, zoom);
    const page = await app.firstWindow();
    const tile = () => page.getByRole('button', { name: 'Draft badge · waiting for your go', exact: true });
    await expect(tile()).toBeVisible();
    await tile().focus(); await tile().press('Control+Enter');
    let menu = page.getByRole('menu', { name: 'Plan item actions for Draft badge', exact: true });
    await expect(menu).toBeVisible(); await expect(menu.getByRole('menuitem', { name: 'Move earlier', exact: true })).toBeFocused();
    const rect = await menu.evaluate(element => { const b = element.getBoundingClientRect(); return { left: b.left, top: b.top, right: b.right, bottom: b.bottom, width: innerWidth, height: innerHeight }; });
    expect(rect.left).toBeGreaterThanOrEqual(0); expect(rect.top).toBeGreaterThanOrEqual(0); expect(rect.right).toBeLessThanOrEqual(rect.width); expect(rect.bottom).toBeLessThanOrEqual(rect.height);
    await writeFile(info.outputPath('plan-item-menu.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await menu.getByRole('menuitem', { name: 'Move later', exact: true }).click();
    const moreA = page.getByRole('button', { name: 'Show 1 more planned items for A', exact: true });
    await expect(moreA).toBeFocused();
    await expect(page.getByRole('status').filter({ hasText: 'Moved to A, after Verify badge.' })).toHaveCount(1);
    await moreA.press('Alt+ArrowLeft');
    await expect(tile()).toBeFocused();
    await tile().press('Alt+ArrowDown');
    const moreB = page.getByRole('button', { name: 'Show 1 more planned items for B', exact: true });
    await expect(moreB).toBeFocused();
    await moreB.press('Control+Enter');
    menu = page.getByRole('menu', { name: 'Plan item actions for Draft badge', exact: true });
    await menu.getByRole('menuitem', { name: 'Change dependency…', exact: true }).click();
    const dependencies = page.getByRole('dialog', { name: 'Change dependencies', exact: true });
    await dependencies.getByRole('checkbox', { name: 'Verify badge', exact: true }).check();
    await writeFile(info.outputPath('dependency-editor.png'), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    await dependencies.getByRole('button', { name: 'Apply dependencies', exact: true }).click();
    await expect(dependencies.getByRole('alert')).toContainText('Saved dependency reply lost');
    await dependencies.getByRole('button', { name: 'Retry dependency action', exact: true }).click();
    const edits = await app.evaluate(() => (globalThis as typeof globalThis & { editPlanFixture: { edits: PlanRewriteAction[] } }).editPlanFixture.edits.filter(action => action.kind === 'dependencies'));
    expect(edits).toHaveLength(2); expect(edits[0]).toEqual(edits[1]);
    await expect(dependencies).toHaveCount(0);
    let state = await app.evaluate(() => (globalThis as typeof globalThis & { editPlanFixture: { plan: Plan; calls: PlanCommand[]; reject: boolean } }).editPlanFixture);
    expect(state.plan.items[0].trackRef).toBe('worker-b');
    expect(state.plan.items[0].after).toEqual([{ id: 'dummy-item-1', inherit: true }]);
    await app.evaluate(() => { (globalThis as typeof globalThis & { editPlanFixture: { reject: boolean } }).editPlanFixture.reject = true; });
    await moreB.focus(); await moreB.press('Alt+ArrowLeft');
    await expect(page.getByRole('alert').filter({ hasText: 'This move would create a dependency cycle.' })).toBeVisible();
    const failed = await app.evaluate(() => (globalThis as typeof globalThis & { editPlanFixture: { calls: PlanCommand[] } }).editPlanFixture.calls.at(-1)!);
    await app.evaluate(() => { (globalThis as typeof globalThis & { editPlanFixture: { reject: boolean } }).editPlanFixture.reject = false; });
    await page.getByRole('button', { name: 'Retry plan action', exact: true }).click();
    state = await app.evaluate(() => (globalThis as typeof globalThis & { editPlanFixture: { plan: Plan; calls: PlanCommand[]; reject: boolean } }).editPlanFixture);
    expect(state.calls.at(-1)).toEqual(failed);
    await page.getByRole('button', { name: 'Undo', exact: true }).click();
    await expect(page.getByRole('region', { name: 'Plans', exact: true })).toBeVisible();
    await page.getByText('Taken back · 1', { exact: true }).click();
    await page.getByRole('button', { name: 'Restore held', exact: true }).click();
    await expect(page.getByText('Taken back · 1', { exact: true })).toHaveCount(0);
    const histories = await app.evaluate(() => (globalThis as typeof globalThis & { editPlanFixture: { histories: HistoryRequest[] } }).editPlanFixture.histories);
    expect(histories.at(-1)?.action).toEqual({ kind: 'restore', utteranceId: 'dummy-taken-back', revision: 2 });
    await moreB.focus(); await moreB.press('Backspace');
    await expect(moreB).toHaveCount(0);
    const guide = page.getByRole('button', { name: 'Write guide · waiting for your go', exact: true });
    await guide.focus(); await guide.press('Backspace');
    await expect(page.getByRole('gridcell', { name: /B · Dummy guide/ })).toHaveCount(0);
    await page.getByRole('button', { name: 'Undo', exact: true }).click();
    await expect(page.getByRole('gridcell', { name: /B · Dummy guide/ })).toBeVisible();
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
