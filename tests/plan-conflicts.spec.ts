import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, Proposal, ProposalCommand, ProposalResponse, PlanReplyRequest, PlanReplyResult, Utterance } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication, page: Page, directory: string;
const card = () => page.locator('[data-attention-card^="proposal-"]');
test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/intercat-plan-conflict-ui-');
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}`] });
  await mockDrafts(app); page = await app.firstWindow();
  await page.clock.install({ time: Date.now() });
  await page.clock.pauseAt(Date.now() + 1000);
});
test.afterEach(async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); });
async function mount(zoom = 1, blocked = false, lose = false, age = 0, replyMode = false) {
  await app.evaluate(({ ipcMain, BrowserWindow }, input) => {
    const proposal: Proposal = { id: 'dummy-conflict', kind: 'plan-conflict', planId: 'dummy-plan', planRevision: 2, replyBlockedReason: null, label: 'New track 1 + New track 2',
      text: 'New track 1 and New track 2 may conflict', createdAt: input.now - input.age,
      detail: 'Both jobs revise the dummy index. Fine keeps the current plan. Run together removes only ordering links between these tracks in this plan; other start conditions still apply.',
      options: [{ action: 'keep-plan', text: 'Fine' }, { action: 'parallel-plan', text: 'Run together',
        disabledReason: input.blocked ? 'These steps depend on inherited files. Review their dependencies before running together.' : null }],
    };
    const state: DaemonStatus = { connected: true, sequence: 1, workerConfigured: true,
      session: { id: 'dummy-session', name: 'Dummy plan conflict', slug: 'conflicts', createdAt: 1 },
      tracks: [], workers: [], utterances: [], proposal };
    if (input.replyMode) state.tracks = [{id:'B',label:'B',title:'Original dummy destination',status:'ready'}];
    const memory = { state, planRequests: [] as PlanReplyRequest[], planReplies: new Map<string, PlanReplyResult>(), requests: [] as ProposalCommand[], replies: new Map<string, ProposalResponse>(), lose: input.lose };
    Object.assign(globalThis, { conflictFixture: memory });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:proposal-answer'); ipcMain.handle('intercat:proposal-answer', (_event, command: ProposalCommand) => {
      memory.requests.push(command);
      if (memory.replies.has(command.opId)) return memory.replies.get(command.opId);
      const response: ProposalResponse = { proposalId: command.proposalId, action: command.action, planId: 'dummy-plan', planRevision: 3 };
      memory.replies.set(command.opId, response);
      state.proposal = null; state.sequence!++;
      if (memory.lose) { memory.lose = false; throw new Error('Dummy lost acknowledgement'); }
      return response;
    });
    ipcMain.removeHandler('intercat:plan-reply'); ipcMain.handle('intercat:plan-reply', (_event, command: PlanReplyRequest) => {
      memory.planRequests.push(command);
      const old = memory.planReplies.get(command.opId); if (old) return old;
      const stale = state.proposal?.id !== command.questionId;
      const result: PlanReplyResult = { inputId: command.opId, planId: command.planId, state: stale ? 'orphaned' : 'landed' };
      const word: Utterance = { id: command.opId, text: command.text, attachments: [], createdAt: input.now, state: result.state,
        revision: 1, reason: stale ? 'the question changed meanwhile' : null, target: { mode: 'none' }, decision: null,
        planReply: { planId: command.planId, question: command.question, state: result.state } };
      state.utterances!.push(word); state.proposal = null; state.sequence!++;
      memory.planReplies.set(command.opId,result);
      if (memory.lose) { memory.lose=false; throw new Error('Dummy lost acknowledgement'); }
      return result;
    });
    BrowserWindow.getAllWindows()[0].setSize(1040, 900);
    BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(input.zoom);
  }, { zoom, blocked, lose, age, replyMode, now: await page.evaluate(() => Date.now()) });
  await page.reload(); await page.clock.runFor(1100);
  await page.mouse.move(5, 5); await page.clock.runFor(1600);
}
for (const zoom of [1, 2]) test(`plan conflict stays readable and offers its choices and a reply at ${zoom*100}%`, async () => {
  await mount(zoom);
  await expect(card()).toContainText('New track 1 and New track 2 may conflict');
  await expect(card().getByRole('button')).toHaveCount(3);
  await card().getByText('Details', { exact: true }).click();
  await expect(card()).toContainText('Both jobs revise the dummy index.');
  for (const button of await card().getByRole('button').all()) {
    await button.scrollIntoViewIfNeeded(); await expect(button).toBeInViewport({ ratio: 1 });
  }
  const detail = card().locator('details p');
  await detail.scrollIntoViewIfNeeded(); await expect(detail).toBeInViewport({ ratio: 1 });
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath(`plan-conflict-${zoom*100}.png`), Buffer.from(pixels, 'base64'));
  await card().getByRole('button', { name: 'Run together', exact: true }).click();
  await expect.poll(() => app.evaluate(() => (globalThis as any).conflictFixture.requests.map((request: ProposalCommand) => request.action))).toEqual(['parallel-plan']);
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1800);
  await expect(card()).toHaveCount(0);
});
test('inherited files prevent parallel order change and explain why beside the choices', async () => {
  await mount(1, true);
  const button = card().getByRole('button', { name: 'Run together', exact: true });
  await expect(button).toBeDisabled();
  await expect(button).toHaveAccessibleDescription('These steps depend on inherited files. Review their dependencies before running together.');
  await card().getByRole('button', { name: 'Fine', exact: true }).click();
  await expect.poll(() => app.evaluate(() => (globalThis as any).conflictFixture.requests.map((request: ProposalCommand) => request.action))).toEqual(['keep-plan']);
});
test('lost acknowledgement retries the same choice and operation identity', async () => {
  await mount(1, false, true);
  await card().getByRole('button', { name: 'Run together', exact: true }).click();
  await expect(card().getByRole('alert')).toContainText('Dummy lost acknowledgement');
  await card().getByRole('button', { name: 'Retry saving choice', exact: true }).click();
  const requests = await app.evaluate(() => (globalThis as any).conflictFixture.requests as ProposalCommand[]);
  expect(requests).toHaveLength(2); expect(requests[0]).toEqual(requests[1]);
  await expect(card()).toContainText('answered elsewhere');
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1800);
  await expect(card()).toHaveCount(0);
});
test('a conflict ignored for ten minutes folds and remains available', async () => {
  await mount(1, false, false, 601_000);
  await expect(card()).toBeHidden();
  await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(card()).toBeVisible();
  await card().getByRole('button', { name: 'Fine', exact: true }).click();
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1800);
  await expect(card()).toHaveCount(0);
});

for (const zoom of [1,2]) test(`plan reply keeps the original destination, survives reload and retries at ${zoom*100}%`, async () => {
  await mount(zoom,false,true,0,true);
  const draft=page.getByRole('textbox',{name:'Request draft',exact:true});
  await draft.fill('Keep this original B draft.');
  await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
  await card().getByRole('button',{name:'Reply…',exact:true}).click();
  await expect(draft).toHaveValue('');
  await expect(page.getByRole('button',{name:'New track',exact:true})).toBeDisabled();
  await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveCount(0);
  await draft.fill('Use separate dummy files in the two steps.');
  await page.getByRole('button',{name:'Cancel reply',exact:true}).click();
  await expect(draft).toHaveValue('Keep this original B draft.');
  await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
  await card().getByRole('button',{name:'Reply…',exact:true}).click();
  await expect(draft).toHaveValue('Use separate dummy files in the two steps.');
  await page.reload(); await page.clock.runFor(1200);
  await expect(draft).toHaveValue('Use separate dummy files in the two steps.');
  const send=page.getByRole('button',{name:'Send reply',exact:false});
  await send.scrollIntoViewIfNeeded(); await expect(send).toBeInViewport({ratio:1});
  await writeFile(test.info().outputPath(`plan-reply-${zoom*100}.png`),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
  await send.click();
  await expect(page.getByRole('button',{name:/Retry save/})).toBeVisible();
  await page.getByRole('button',{name:/Retry save/}).click();
  await expect(draft).toHaveValue('Keep this original B draft.');
  await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
  const requests=await app.evaluate(()=>(globalThis as any).conflictFixture.planRequests as PlanReplyRequest[]);
  expect(requests).toHaveLength(2); expect(requests[0]).toEqual(requests[1]);
  expect(requests[0]).toMatchObject({planId:'dummy-plan',questionId:'dummy-conflict',revision:2,text:'Use separate dummy files in the two steps.'});
});

test('a stale plan reply becomes an unsent request with its original question', async () => {
  await mount(1,false,false,0,true);
  const draft=page.getByRole('textbox',{name:'Request draft',exact:true});
  await draft.fill('Keep the ordinary dummy draft.');
  await card().getByRole('button',{name:'Reply…',exact:true}).click();
  await draft.fill('Keep this stale dummy reply for review.');
  await app.evaluate(()=>{const fixture=(globalThis as any).conflictFixture; fixture.state.proposal=null; fixture.state.sequence++;});
  await page.getByRole('button',{name:'Send reply',exact:false}).click();
  await expect(draft).toHaveValue('Keep the ordinary dummy draft.');
  await page.mouse.move(5,5); await page.clock.runFor(2000);
  const orphan=page.getByRole('group',{name:'Reply needs a destination'});
  await expect(orphan).toContainText('the question changed meanwhile');
  await orphan.getByText('Original question',{exact:true}).click();
  await expect(orphan).toContainText('Both jobs revise the dummy index.');
  const requests=await app.evaluate(()=>(globalThis as any).conflictFixture.planRequests as PlanReplyRequest[]);
  expect(requests).toHaveLength(1);
});

test('an oversized plan reply stays editable without creating a pending operation', async () => {
  await mount(1,false,false,0,true);
  await card().getByRole('button',{name:'Reply…',exact:true}).click();
  const draft=page.getByRole('textbox',{name:'Request draft',exact:true});
  const text='あ'.repeat(11000);await draft.fill(text);
  await page.getByRole('button',{name:'Send reply',exact:false}).click();
  await expect(draft).toBeEnabled();await expect(draft).toHaveValue(text);
  await expect(page.getByRole('status').filter({hasText:'Plan replies can contain up to 32 KiB'})).toBeVisible();
  expect(await app.evaluate(()=>(globalThis as any).conflictFixture.planRequests.length)).toBe(0);
  await expect(page.getByRole('button',{name:/Retry save/})).toHaveCount(0);
});
