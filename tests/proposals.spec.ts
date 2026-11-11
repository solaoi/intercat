import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, Proposal, ProposalCommand, ProposalResponse, Utterance } from '../src/bridge';
import type { IntegrationJob } from '../src/commit/types';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication, page: Page, directory: string;
const card = () => page.locator('[data-attention-card^="proposal-"]');

test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/intercat-proposal-ui-');
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}`] });
  await mockDrafts(app); page = await app.firstWindow();
  await page.clock.install({ time: Date.now() });
  await page.clock.pauseAt(Date.now() + 1000);
});
test.afterEach(async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); });

async function mount(kind: 'done' | 'partial' | 'quiet' | 'overlap' | 'fix', zoom = 1, age = 0, lose = false, bound = false) {
  await app.evaluate(({ ipcMain, BrowserWindow }, input) => {
    const proposal: Proposal = { id: 'dummy-proposal', kind: input.kind === 'quiet' ? 'quiet-changes' : 'work-completed', trackId: 'A', label: 'A',
      createdAt: input.now - input.age,
      text: input.kind === 'quiet' ? 'A has uncommitted changes — review ダミーの作業?' : input.kind === 'done' ? 'A is done — commit the dummy login work now?' : 'A finished part of it',
      detail: input.kind === 'quiet' ? 'No work or draft edits were recorded for five minutes. The saved changes differ from the last integrated version.' : 'Worker report: ダミーの検証作業です。Review the changes and checks before committing.',
      options: input.kind !== 'partial' ? [{ action: 'commit', text: 'Review for commit' }, { action: 'later', text: 'Not now' }]
        : [{ action: 'continue', text: 'Ask A to continue' }, { action: 'commit', text: 'Commit what’s there' }],
    };
    if(input.kind==='fix') {
      proposal.kind='integration-fix';proposal.text='Integration fixed A’s committed changes · Realign?';
      proposal.detail='The dummy integration fix is committed. Worker A’s files have not changed.';
      proposal.options=[{action:'realign',text:'Realign A'},{action:'later',text:'Not now'}];
    }
    if (input.kind === 'overlap') {
      proposal.kind = 'work-overlap'; proposal.label = 'A + B'; proposal.text = 'A and B touch the same files';
      proposal.detail = 'Five quiet minutes. Saved changes overlap in shared.ts. Review the track order before committing.';
      proposal.options[0].text = 'Review order';
    }
    const state: DaemonStatus = { connected: true, sequence: 1, workerConfigured: true,
      session: { id: 'dummy-session', name: 'Dummy proposal test', slug: 'proposals', createdAt: 1 },
      tracks: [{ id: 'A', label: 'A', title: 'ダミーのログイン作業', status: 'done' }], workers: [], utterances: [], proposal,
    };
    if (input.kind === 'overlap') state.tracks!.push({ id: 'B', label: 'B', title: '別のダミー作業', status: 'done' });
    if (input.bound) state.plans = [{ id:'dummy-plan',rev:1,state:'running',sourceId:'source',sourceRevision:1,startMode:'manual',executeAllowed:true,lockedTrack:null,
      generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Dummy remainder',assumptions: [], notices:[],seenRevision:null,rewrite:null,
      items:[{planItemId:'dummy-item',trackRef:'A',folderId:'dummy',order:0,title:'Dummy remainder',instruction:'Finish the dummy work.',constraints:[],doneWhen:'Done.',after:[],state:'failed',skipOrder:[],startRequested:false,utteranceId:'initial',detail:'One step remains.'}]}];
    const memory = { state, requests: [] as ProposalCommand[], integrations: [] as unknown[], replies: new Map<string, ProposalResponse>(), lose: input.lose };
    Object.assign(globalThis, { proposalFixture: memory });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:proposal-answer'); ipcMain.handle('intercat:proposal-answer', (_event, command: ProposalCommand) => {
      memory.requests.push(command);
      if (memory.replies.has(command.opId)) return memory.replies.get(command.opId);
      const response: ProposalResponse = { proposalId: command.proposalId, action: command.action, trackId: 'A' };
      if (command.action === 'commit') {
        response.inputId = 'dummy-review';
        const word: Utterance = { id: response.inputId, text: 'Review Worker A for commit.', attachments: [], createdAt: input.now,
          state: 'placed', revision: 1, reason: null, target: { mode: 'lock', trackId: 'A' }, decision: null,
          presentation: { command: 'commit', operationId: 'dummy-open-review', receiptId: 'dummy-receipt', trackId: 'A' } };
        if (input.kind === 'overlap') word.presentation!.commitTracks = ['A', 'B'];
        state.utterances!.push(word);
      }
      if(command.action==='realign') {
        response.realignment={request:{opId:'dummy-realign-request',trackId:'A',source:{kind:'commit',jobId:'dummy-commit',revision:2}},sessionId:'dummy-worker-session',revision:1,phase:'queued',mergeInput:null,mergeBoundary:null,sharedInput:null,resolutions:[],keepProcesses:false,waitForAccess:false,stopCommand:null,stopCheckpoint:null,inspection:null,error:null};
        state.realignments=[response.realignment];
      }
      if (command.action === 'continue') {
        response.inputId = 'dummy-continuation';
        if (input.bound) state.utterances!.push({id:response.inputId,text:'Internal dummy continuation instructions.',attachments:[],createdAt:input.now,
          state:'held',revision:1,reason:'Turns stopped',target:{mode:'lock',trackId:'A'},planId:'dummy-plan',planItemId:'dummy-item',planContinuation:{title:'Dummy remainder'},
          decision:{kind:'extend',trackId:'A',boundary:'after-turn',command:null,confidence:1,inferenceMs:0,timedOut:false}});
      }
      memory.replies.set(command.opId, response); state.proposal = null; state.sequence!++;
      if (memory.lose) { memory.lose = false; throw new Error('Dummy lost acknowledgement'); }
      return response;
    });
    ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', (_event, id) => state.utterances!.find(word => word.id === id));
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, request) => {
      const word = state.utterances!.find(word => word.id === request.utteranceId)!;
      word.state = 'landed'; word.presentation = undefined; state.sequence!++; return word;
    });
    ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request) => {
      memory.integrations.push(request);
      const job: IntegrationJob = { id: 'dummy-job', revision: 1, status: 'open', sessionName: 'Dummy review', slug: 'dummy',
        project: { id: 'dummy-folder', name: 'Dummy', root: '/dummy', path: '/dummy', commonDir: '/dummy/.git', base: 'a'.repeat(40), baseBranch: 'refs/heads/main' },
        pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions: [], assumptions: [], assumptionsChanged: false, assumptionsPending: 0, assumptionsFailed: 0,
        tracks: [], plan: null, candidate: null, messages: {}, keptSecrets: [], contributions: {}, error: null };
      state.integrations = [job]; return job;
    });
    BrowserWindow.getAllWindows()[0].setSize(1040, 900);
    BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(input.zoom);
  }, { kind, zoom, age, now: await page.evaluate(() => Date.now()), lose, bound });
  await page.reload(); await page.clock.runFor(1100);
  await page.mouse.move(5, 5); await page.clock.runFor(1600);
}

for (const zoom of [1, 2]) test(`partial proposal has two usable actions and visible Japanese detail at ${zoom * 100}%`, async () => {
  await mount('partial', zoom);
  await expect(card()).toContainText('not started');
  await expect(card().getByRole('button')).toHaveCount(2);
  await card().getByText('Details', { exact: true }).click();
  await expect(card()).toContainText('ダミーの検証作業です。');
  for (const button of await card().getByRole('button').all()) {
    await button.scrollIntoViewIfNeeded(); await expect(button).toBeInViewport({ ratio: 1 });
    expect(await button.evaluate(element => {
      const button = element.getBoundingClientRect();
      const scroll = element.closest('.desk-scroll')!.getBoundingClientRect();
      return button.top >= scroll.top && button.bottom <= scroll.bottom;
    })).toBe(true);
  }
  const actions = card().locator('[data-card-actions]');
  const bounds = await actions.boundingBox(); expect(bounds).not.toBeNull();
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath(`partial-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
  await card().getByRole('button', { name: 'Ask A to continue', exact: true }).click();
  await expect.poll(() => app.evaluate(() => (globalThis as any).proposalFixture.requests.map((request: ProposalCommand) => request.action))).toEqual(['continue']);
  expect(await app.evaluate(() => (globalThis as any).proposalFixture.integrations)).toEqual([]);
});

for (const zoom of [1,2]) test(`a held continuation stays visible with its item name at ${zoom*100}%`,async()=>{
  await mount('partial',zoom,0,false,true);
  await card().getByRole('button',{name:'Ask A to continue',exact:true}).click();
  await page.clock.runFor(1200);
  const request=page.locator('.request-strip [data-utterance-id="dummy-continuation"]');
  await expect(request).toBeVisible();
  await expect(page.locator('.request-strip')).not.toContainText('Internal dummy');
  await expect(page.locator('.request-strip').getByRole('button',{name:'Start request: About Dummy remainder',exact:true})).toBeEnabled();
  await app.evaluate(()=>{
    const state:DaemonStatus=(globalThis as any).proposalFixture.state;
    state.utterances![0].state='placed';state.utterances![0].reason=null;state.sequence!++;
  });
  await page.clock.runFor(1200);
  await expect(page.getByRole('button',{name:'Cut About Dummy remainder from A',exact:true})).toBeAttached();
});

for (const lose of [false, true]) test(`commit proposal opens a durable review without publishing${lose ? ' after a lost answer' : ''}`, async () => {
  await mount('done', 1, 0, lose);
  await card().getByRole('button', { name: 'Review for commit', exact: true }).click();
  await page.clock.runFor(2000);
  await expect(page.locator('.commit-sheet')).toBeVisible();
  expect(await app.evaluate(() => (globalThis as any).proposalFixture.integrations)).toEqual([
    { opId: 'dummy-open-review', action: { action: 'open', tracks: ['A'] } },
  ]);
});

test('a quiet overlap proposal opens a review for the complete saved group', async () => {
  await mount('overlap');
  await card().getByRole('button', { name: 'Review order', exact: true }).click();
  await page.clock.runFor(2000);
  await expect(page.locator('.commit-sheet')).toBeVisible();
  expect(await app.evaluate(() => (globalThis as any).proposalFixture.integrations)).toEqual([
    { opId: 'dummy-open-review', action: { action: 'open', tracks: ['A', 'B'] } },
  ]);
});

test('an old proposal is folded but remains reachable', async () => {
  await mount('done', 1, 601_000);
  await expect(card()).toBeHidden();
  await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(card()).toBeVisible();
  await card().getByRole('button', { name: 'Not now', exact: true }).click();
  await page.clock.runFor(1200);
  expect(await app.evaluate(() => (globalThis as any).proposalFixture.requests.map((request: ProposalCommand) => request.action))).toEqual(['later']);
});

test('expiry keeps focused actions in place until focus and pointer leave', async () => {
  await mount('done', 1, 590_000);
  await card().getByRole('button', { name: 'Not now', exact: true }).focus();
  await page.clock.runFor(20_000);
  await expect(card()).toBeVisible();
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await expect(card()).toBeHidden();
  await expect(page.getByRole('button', { name: /\+1 more/ })).toBeVisible();
});

for (const zoom of [1, 2]) test(`proposal, question and assumption keep their priority at ${zoom * 100}%`, async () => {
  await mount('partial', zoom);
  await app.evaluate(() => {
    const state: DaemonStatus = (globalThis as any).proposalFixture.state;
    state.utterances = [{ id: 'dummy-question', text: 'ダミーの配置を選んでください', state: 'asked', revision: 1, attachments: [], createdAt: 1,
      target: { mode: 'none' }, decision: null, reason: 'Choose a destination.' }];
    state.assumptions = [{ id: 'dummy-assumption', source: 'default-placement', sourceId: 'dummy-source', trackId: 'A', clipId: 'dummy-clip',
      question: 'ダミーの保存形式は何ですか', text: 'Use JSON', options: ['Use JSON', 'Use text'], evidence: 'Use JSON for the dummy output.',
      revision: 1, state: 'open', keptAt: null, changedAt: null, changeInputId: null }];
    state.sequence!++;
  });
  await page.clock.runFor(1100);
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await page.getByRole('button', { name: /\+\d+ more/ }).click();
  const order = await page.locator('[data-attention-card]:visible').evaluateAll(elements => elements.map(element => (element as HTMLElement).dataset.attentionCard));
  expect(order).toEqual(['assumption-dummy-assumption', 'proposal-dummy-proposal', 'request-dummy-question']);
  await expect(card()).toContainText('not started');
  await expect(page.locator('[data-attention-card="assumption-dummy-assumption"] details')).not.toHaveAttribute('open');
  await card().getByRole('button', { name: 'Ask A to continue', exact: true }).focus();
  await expect(card().getByRole('button', { name: 'Ask A to continue', exact: true })).toBeInViewport({ ratio: 1 });
});

test('returning from absence folds the existing proposal until explicitly opened', async () => {
  await mount('done');
  const now = await page.evaluate(() => Date.now());
  await app.evaluate(({ BrowserWindow, ipcMain }, now) => {
    ipcMain.removeHandler('intercat:editor-away');
    ipcMain.handle('intercat:editor-away', () => ({ updates: 0, effects: 0, clips: 0, waiting: [], entries: [], before: null }));
    ipcMain.removeHandler('intercat:editor-return-read'); ipcMain.handle('intercat:editor-return-read', () => null);
    BrowserWindow.getAllWindows()[0].webContents.send('intercat:editor-presentation-changed', {
      character: 'speak', hidden: false, returned: { id: 'dummy-return', from: now - 700_000, to: now },
    });
  }, now);
  await page.clock.runFor(1100);
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await expect(card()).toBeHidden();
  await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(card()).toBeVisible();
});

test('a changed title cannot move the action under the pointer', async () => {
  await mount('done');
  await card().getByRole('button', { name: 'Not now', exact: true }).hover();
  const old = await card().boundingBox();
  await app.evaluate(() => {
    const state: DaemonStatus = (globalThis as any).proposalFixture.state;
    state.proposal!.text = 'A is done — commit the updated dummy title and its longer explanation now?'; state.sequence!++;
  });
  await page.clock.runFor(1100);
  await expect(card()).toContainText('changed elsewhere');
  expect((await card().boundingBox())?.height).toBe(old?.height);
  await expect(card().locator('.desk-actions button').first()).toBeDisabled();
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await expect(card()).toContainText('updated dummy title');
});

for (const zoom of [1, 2]) test(`quiet changes open a review and do not claim completion at ${zoom * 100}%`, async () => {
  await mount('quiet', zoom);
  await expect(card()).toContainText('has uncommitted changes');
  await expect(card()).not.toContainText('is done');
  await card().getByText('Details', { exact: true }).click();
  await expect(card()).toContainText('five minutes');
  const button = card().getByRole('button', { name: 'Review for commit', exact: true });
  await button.scrollIntoViewIfNeeded(); await expect(button).toBeInViewport({ ratio: 1 });
  expect(await button.evaluate(element => element.getBoundingClientRect().bottom <= element.closest('.desk-scroll')!.getBoundingClientRect().bottom)).toBe(true);
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath(`quiet-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
  await button.click(); await page.clock.runFor(2000);
  await expect(page.locator('.commit-sheet')).toBeVisible();
  expect(await app.evaluate(() => (globalThis as any).proposalFixture.integrations)).toEqual([
    { opId: 'dummy-open-review', action: { action: 'open', tracks: ['A'] } },
  ]);
});

for (const zoom of [1, 2]) test(`mixed M2 answers preserve a locked draft through reload at ${zoom * 100}%`, async () => {
  await mount('done', zoom);
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const memory = (globalThis as any).proposalFixture;
    const state: DaemonStatus = memory.state;
    memory.answers = [];
    state.tracks!.push({ id: 'B', label: 'B', title: '下書きの宛先', status: 'ready' },
      { id: 'C', label: 'C', title: '新しい質問', status: 'ready' });
    state.utterances = [{ id: 'mixed-unplaced', text: 'この変更をどこへ置くか選んでください。', attachments: [],
      createdAt: 1, state: 'asked', revision: 1, target: { mode: 'none' }, decision: null, reason: 'Choose a destination.' }];
    state.assumptions = [{ id: 'mixed-assumption', source: 'default-placement', sourceId: 'mixed-source', trackId: 'A', clipId: 'mixed-clip',
      question: '一覧の保存形式は何にしますか。', text: 'Use JSON', options: ['Use JSON', 'Use text'], evidence: 'ダミーの出力をJSONで保存しました。',
      revision: 1, state: 'open', keptAt: null, changedAt: null, changeInputId: null }];
    ipcMain.removeHandler('intercat:worker-respond'); ipcMain.handle('intercat:worker-respond', (_event, command) => {
      memory.answers.push(command); state.workers![0].prompts = []; state.sequence!++;
    });
    ipcMain.removeHandler('intercat:assumption-answer'); ipcMain.handle('intercat:assumption-answer', (_event, command) => {
      memory.answers.push(command); const choice = state.assumptions![0];
      state.assumptions = []; state.sequence!++; return { ...choice, state: 'kept' };
    });
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, command) => {
      memory.answers.push(command); const word = state.utterances![0];
      word.state = 'held'; word.revision++; state.sequence!++; return word;
    });
    state.sequence!++; BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  });
  await page.clock.runFor(1100);
  const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
  const destination = page.getByRole('combobox', { name: 'Request destination' });
  const text = 'Bへの依頼を書きかけたまま、AとCの確認に答えます。';
  await draft.fill(text); await destination.selectOption('B');
  // Row clicks open the worker details and return to the normal desk layout.
  await draft.focus(); await page.keyboard.press('Meta+Shift+T');
  await expect(page.locator('.app')).toHaveClass(/stage-mode/);
  await page.locator('#track-B').click();
  await expect(page.locator('.app')).not.toHaveClass(/stage-mode/);
  await draft.focus(); await page.keyboard.press('Meta+Shift+T');
  await expect(page.locator('.app')).toHaveClass(/stage-mode/);
  await page.clock.runFor(200);
  await page.getByRole('scrollbar', { name: 'All tracks', exact: true }).press('Home');
  await page.locator('#track-A').click();
  await expect(page.locator('.app')).not.toHaveClass(/stage-mode/);
  await draft.focus(); await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await page.getByRole('button', { name: /\+\d+ more/ }).click();
  const action = card().getByRole('button', { name: 'Review for commit', exact: true });
  await action.focus(); const before = (await action.boundingBox())!;
  await app.evaluate(() => {
    const state: DaemonStatus = (globalThis as any).proposalFixture.state;
    state.workers = [{ trackId: 'C', phase: 'question', sessionId: 'mixed-C', activeTurn: 'mixed-turn', identity: null,
      detail: null, checkpointPending: false, checkpoint: null, approvals: [], output: null,
      prompts: [{ request: { sessionId: 'mixed-C', userInputId: 'mixed-prompt', turnId: 'mixed-turn',
        questions: [1, 2, 3].map(index => ({ id: `question-${index}`, header: `保存形式 ${index}`,
          question: 'ダミーの確認項目です。形式を選び、書きかけの依頼の宛先を保ってください。'.repeat(3),
          selection: { mode: 'single' as const }, options: [{ label: `JSON ${index}` }, { label: `TSV ${index}` }] })) },
        revision: '1', phase: 'open', operation: null }] }];
    state.sequence!++;
  });
  await page.clock.runFor(1100);
  await expect(action).toBeFocused();
  expect(Math.abs((await action.boundingBox())!.y - before.y)).toBeLessThan(1);
  await expect(page.getByText('+1 new', { exact: true })).toBeVisible();
  await draft.focus(); await page.mouse.move(5, 5); await page.clock.runFor(1600);
  const prompt = page.getByRole('group', { name: 'Questions for Worker C', exact: true });
  for (let index = 1; index <= 3; index++) await prompt.getByRole('radio', { name: `JSON ${index}`, exact: true }).check();
  const send = prompt.getByRole('button', { name: 'Send answers', exact: true });
  await send.scrollIntoViewIfNeeded(); await expect(send).toBeInViewport({ ratio: 1 });
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath(`mixed-m2-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
  await send.click(); await page.clock.runFor(1100);
  await draft.focus(); await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await page.locator('[data-attention-card="request-mixed-unplaced"]').getByRole('button', { name: 'Hold', exact: true }).click();
  await page.clock.runFor(1100);
  await draft.focus(); await page.mouse.move(5, 5); await page.clock.runFor(1600);
  const assumption = page.locator('[data-attention-card="assumption-mixed-assumption"]');
  await assumption.locator('summary').click(); await assumption.getByRole('button', { name: 'Keep', exact: true }).click();
  await page.clock.runFor(1100);
  await draft.focus(); await page.mouse.move(5, 5); await page.clock.runFor(1600);
  await card().getByRole('button', { name: 'Not now', exact: true }).click();
  await expect(draft).toHaveValue(text); await expect(destination).toHaveValue('B');
  const result = await app.evaluate(() => {
    const memory = (globalThis as any).proposalFixture;
    return { answers: memory.answers, proposals: memory.requests, integrations: memory.integrations };
  });
  expect(result.answers).toEqual([
    expect.objectContaining({ trackId: 'C', promptId: 'mixed-prompt', revision: '1', response: { action: 'answer',
      answers: [1, 2, 3].map(index => ({ questionId: `question-${index}`, selectedLabel: `JSON ${index}` })) } }),
    expect.objectContaining({ utteranceId: 'mixed-unplaced', resolution: { action: 'hold' } }),
    expect.objectContaining({ assumptionId: 'mixed-assumption', answer: { action: 'keep' } }),
  ]);
  expect(result.proposals).toEqual([expect.objectContaining({ proposalId: 'dummy-proposal', action: 'later' })]);
  expect(result.integrations).toEqual([]);
  await page.reload(); await page.clock.runFor(1100);
  await expect(draft).toHaveValue(text); await expect(destination).toHaveValue('B');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});

for(const zoom of [1,2])test(`integration fix proposes explicit realignment and retries the saved choice at ${zoom*100}%`,async()=>{
  await mount('fix',zoom,0,true);
  await expect(card()).toContainText('Integration fixed A’s committed changes');
  await expect(card().getByRole('button',{name:'Realign A',exact:true})).toBeInViewport({ratio:1});
  await page.clock.runFor(100);
  const output=process.cwd()+'/research/verify/m4-integration-followup-ui-20261109-01';await mkdir(output,{recursive:true});
  const pixels=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
  await writeFile(`${output}/proposal-${zoom*100}.png`,Buffer.from(pixels,'base64'));

  await card().getByRole('button',{name:'Realign A',exact:true}).click();
  await expect(card().getByRole('alert')).toContainText('Dummy lost acknowledgement');
  await card().getByRole('button',{name:'Retry saving choice',exact:true}).click();
  await page.getByRole('textbox',{name:'Request draft'}).focus();await page.mouse.move(5,5);await page.clock.fastForward(2000);
  await expect(page.locator('.realignment')).toContainText('Realignment queued for A');
  const saved=await app.evaluate(()=>(globalThis as any).proposalFixture);
  expect(saved.requests).toHaveLength(2);expect(saved.requests[0]).toEqual(saved.requests[1]);expect(saved.requests[0].action).toBe('realign');
  expect(saved.integrations).toEqual([]);expect(saved.state.utterances).toEqual([]);
});
