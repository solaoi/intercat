import { closeWindow } from './support/close';
import { timelineClips } from './support/timeline';
import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';
import type { IntegrationJob } from '../src/commit/types';
import type { RepairRequest, RepairRun } from '../src/commit/repair';

let app: ElectronApplication;
let data: string;
let daemon: ChildProcess | undefined;
let evidenceDatabase: { source: string; target: string } | undefined;
test.beforeEach(async () => {
  data = await mkdtemp(path.join(tmpdir(), 'intercat-commit-ui-'));
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${path.join(data, 'ui')}`], env: Object.fromEntries(Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined)) });
  await app.firstWindow();
});
test.afterEach(async () => {
  if (app) await closeWindow(app);
  if (daemon && daemon.exitCode === null && daemon.signalCode === null) { const stopped = new Promise(resolve => daemon!.once('exit', resolve)); daemon.kill('SIGTERM'); await stopped; daemon = undefined; }
  if (evidenceDatabase) { execFileSync('python3', ['-c', 'import sqlite3,sys; source=sqlite3.connect(sys.argv[1]); target=sqlite3.connect(sys.argv[2]); source.backup(target); target.close(); source.close()', evidenceDatabase.source, evidenceDatabase.target]); evidenceDatabase = undefined; }
  await rm(data, { recursive: true, force: true });
});

function fixture(): IntegrationJob {
  const changes = [{ path: 'index.js', before: { object: 'b'.repeat(40), mode: 33188 }, after: { object: 'c'.repeat(40), mode: 33188 } }];
  return { id: '01a1a200-0000-7000-8000-000000000001', revision: 1, status: 'checked', sessionName: 'Commit fixture', slug: 'commit-fixture', project: { name: 'Fixture', root: '/fixture', path: '/fixture', commonDir: '/fixture/.git', base: 'a'.repeat(40), baseBranch: 'refs/heads/main' }, error: null,
    tracks: [{ id: '01a1a200-0000-7000-8000-000000000002', label: 'A', title: 'Update the exported value', included: true, checkpointCount: 1, lastClip: null, checkpoint: { sha: 'c'.repeat(40) } }],
    plan: { targetRef: 'refs/heads/intercat/commit-fixture', tree: 'd'.repeat(40), rows: [{ track: '01a1a200-0000-7000-8000-000000000002', state: 'included', changes, conflicts: [] }] }, messages: {}, keptSecrets: [], contributions: {},
    candidate: { revision: 1, state: 'checked', generation: 1, head: 'e'.repeat(40), tree: 'd'.repeat(40), settingsHash: 'f'.repeat(64), commits: [{ track: '01a1a200-0000-7000-8000-000000000002', sha: 'e'.repeat(40), parent: 'a'.repeat(40), tree: 'd'.repeat(40), author: 'Worker A', message: 'Update the exported value\n\nIntercat: fixture/A@1\n', changes }], blockedCommit: null, secretScans: [], check: { command: 'npm test', code: 0, stdout: 'Check passed\n', stderr: '', truncated: false }, reason: null, failedTrack: null, hooksSkipped: false, dependencyRuns: [] } };
}

async function mock(job: IntegrationJob, loseKeep = false) {
  await app.evaluate(({ ipcMain, BrowserWindow }, input) => {
    const state = { connected: true, sequence: 1, workerConfigured: true, project: input.job.project, session: { id: 'session', slug: input.job.slug, name: input.job.sessionName, createdAt: 0 }, tracks: input.job.tracks.map(track => ({ ...track, status: 'done' })), workers: [], workspaces: [], utterances: [], editor: { phase: 'ready' }, integration: input.job };
    const memory = globalThis as typeof globalThis & { commitCalls: unknown[]; commitState: typeof state; commitClips: unknown[] };
    memory.commitCalls = []; memory.commitState = state; memory.commitClips = [];
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: memory.commitClips, removed: [], marks: [], removedMarks: [] }));
    const replies = new Map<string, unknown>();
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:output-flows'); ipcMain.handle('intercat:output-flows', () => (state as typeof state & { outputFlows?: unknown[] }).outputFlows ?? []);
    ipcMain.removeHandler('intercat:integration-diff'); ipcMain.handle('intercat:integration-diff', () => ({ path: 'index.js', before: 'exports.value = 1;\n', after: 'exports.value = 2;\n', redacted: false, binary: false, tooLarge: false }));
    ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request) => {
      memory.commitCalls.push(request);
      if (replies.has(request.opId)) return replies.get(request.opId);
      if (request.action.action === 'refresh') state.integration.contributions = {};
      if (request.action.action === 'editMessage') { state.integration.messages[request.action.track] = request.action.message; state.integration.revision++; state.integration.candidate!.head = '1'.repeat(40); }
      if (request.action.action === 'keepSecret') { state.integration.keptSecrets.push(request.action.finding); state.integration.revision++; state.integration.status = 'checked'; state.integration.candidate!.state = 'checked'; state.integration.candidate!.blockedCommit = null; }
      if (request.action.action === 'commit') state.integration.status = 'done';
      state.sequence++;
      replies.set(request.opId, structuredClone(state.integration));
      if (request.action.action === 'keepSecret' && input.loseKeep) throw new Error('Response lost after saving');
      return state.integration;
    });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, { job, loseKeep });
}

async function repairHandlers(loseStart = false, loseFollowup = false, loseRefresh = false) {
  await app.evaluate(({ ipcMain }, losses) => {
    const memory = globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; sequence: number; repairRuns: RepairRun[]; workers: unknown[] }; repairCalls: { method: string; request: Record<string, unknown> }[] };
    const state = memory.commitState;
    state.repairRuns = []; memory.repairCalls = [];
    const replies = new Map<string, RepairRun>();
    ipcMain.removeHandler('intercat:repair-start'); ipcMain.handle('intercat:repair-start', (_event, request) => {
      memory.repairCalls.push({ method: 'start', request });
      if (replies.has(request.opId)) return { status: 'accepted', run: replies.get(request.opId) };
      if (request.revision !== state.integration.revision) return { status: 'rejected', reason: 'The candidate changed. Review it again.' };
      state.integration.revision++;
      const run: RepairRun = { id: '01a1a200-0000-5000-8000-000000000099', revision: 1, jobId: request.jobId, jobRevision: state.integration.revision, track: request.track, purpose: request.purpose, phase: 'running', approvals: [], prompts: [], output: null, failure: null, session: 'repair-session', turn: '01a1a200-0000-7000-8000-000000000077', secretInput: null, refresh: null, restoring: null };
      state.repairRuns = [run]; state.sequence++;
      replies.set(request.opId, structuredClone(run));
      if (losses.loseStart) throw new Error('Repair response lost after saving');
      return { status: 'accepted', run };
    });
    for (const method of ['stop', 'control', 'answer', 'respond', 'followup']) {
      ipcMain.removeHandler(`intercat:repair-${method}`); ipcMain.handle(`intercat:repair-${method}`, (_event, request) => {
        memory.repairCalls.push({ method, request });
        const run = state.repairRuns[0];
        if (request.id !== run.id) throw new Error('Wrong repair target');
        if (method === 'followup' && replies.has(request.opId)) return { status: 'accepted', run: replies.get(request.opId) };
        if (method === 'stop') run.phase = 'held';
        if (method === 'control') run.phase = request.action === 'resume' ? 'queued' : 'discarded';
        if (method === 'answer') run.approvals = [];
        if (method === 'respond') run.prompts = [];
        if (method === 'followup') run.phase = 'queued';
        run.revision++; state.sequence++;
        if (method === 'followup') {
          replies.set(request.opId, structuredClone(run));
          if (losses.loseFollowup) throw new Error('Followup response lost after saving');
        }
        return method === 'followup' ? { status: 'accepted', run: structuredClone(run) } : structuredClone(run);
      });
    }
    ipcMain.removeHandler('intercat:repair-refresh'); ipcMain.handle('intercat:repair-refresh', (_event, request) => {
      memory.repairCalls.push({ method: 'refresh', request });
      if (replies.has(request.opId)) return { status: 'accepted', run: replies.get(request.opId) };
      const run = state.repairRuns[0];
      if (request.jobId !== state.integration.id || request.revision !== state.integration.revision || request.repairId !== run.id) return { status: 'rejected', reason: 'The review changed. Confirm Refresh again.' };
      run.refresh = { operation: request.opId, error: null }; run.phase = 'stopping'; state.sequence++;
      replies.set(request.opId, structuredClone(run));
      setTimeout(() => { run.phase = 'discarded'; run.refresh = null; state.integration.revision++; state.integration.contributions = {}; state.integration.status = 'checked'; state.integration.candidate!.head = '2'.repeat(40); state.integration.candidate!.tree = '3'.repeat(40); state.sequence++; }, 100);
      if (losses.loseRefresh) throw new Error('Refresh response lost after saving');
      return { status: 'accepted', run: structuredClone(run) };
    });
    ipcMain.removeHandler('intercat:worker-stop'); ipcMain.handle('intercat:worker-stop', (_event, request) => { memory.repairCalls.push({ method: 'work-stop', request }); });
    ipcMain.removeHandler('intercat:clip-detail'); ipcMain.handle('intercat:clip-detail', () => { throw new Error('No detail in this layout fixture'); });
  }, { loseStart, loseFollowup, loseRefresh });
}

async function selectDiffLine(page: Page) {
  const line = page.locator('.monaco-diff-editor .modified .view-line').first();
  await expect(line).toContainText('exports.value');
  await line.click();
  await page.keyboard.press('Home');
  await page.keyboard.press('Shift+End');
  await expect(page.getByLabel('Commit context')).toContainText('index.js:1');
}

test('selected checked diff starts an isolated repair and replays a lost acknowledgement', async () => {
  const job = fixture(); await mock(job); await repairHandlers(true);
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Return three here.');
  await page.getByRole('button', { name: 'Hide commit review' }).click();
  await expect(page.getByLabel('Commit context')).toContainText('fix for this commit · A');
  await expect(page.getByRole('button', { name: 'New track', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(page.getByRole('button', { name: /Retry save/ })).toBeEnabled();
  await page.getByRole('button', { name: /Retry save/ }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls);
  const starts = calls.filter(call => call.method === 'start');
  expect(starts).toHaveLength(2); expect(starts[0].request).toEqual(starts[1].request);
  expect(starts[0].request.purpose).toEqual({ kind: 'edit', text: 'Return three here.', selection: { tree: job.candidate!.tree, path: 'index.js', object: 'c'.repeat(40), side: 'after', startLine: 1, endLine: 1 } });
});

test('changed commit keeps the draft and reanchors its selected content before resend', async () => {
  await mock(fixture()); await repairHandlers(); const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click(); await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Return three here.');
  await app.evaluate(({ ipcMain }) => {
    const state = (globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; sequence: number } }).commitState;
    state.integration.revision++; state.integration.candidate!.tree = 'e'.repeat(40);
    state.integration.candidate!.commits[0].changes[0].after!.object = 'f'.repeat(40); state.sequence++;
    ipcMain.removeHandler('intercat:integration-diff'); ipcMain.handle('intercat:integration-diff', () => ({ path: 'index.js', before: 'exports.value = 1;\n', after: '// moved\nexports.value = 2;\n', redacted: false, binary: false, tooLarge: false }));
  });
  await expect(page.getByLabel('Commit context')).toContainText('the commit changed');
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Return three here.');
  await page.getByRole('button', { name: 'Resend on the new version' }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const request = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { request: RepairRequest }[] }).repairCalls[0].request);
  expect(request.revision).toBe(2); expect(request.purpose).toMatchObject({ kind: 'edit', selection: { tree: 'e'.repeat(40), object: 'f'.repeat(40), startLine: 2, endLine: 2 } });
});

test('ambiguous selected lines require a new selection and never start a repair', async () => {
  await mock(fixture()); await repairHandlers(); const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click(); await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Change this value.');
  await app.evaluate(({ ipcMain }) => {
    const state = (globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; sequence: number } }).commitState;
    state.integration.revision++; state.sequence++;
    ipcMain.removeHandler('intercat:integration-diff'); ipcMain.handle('intercat:integration-diff', () => ({ path: 'index.js', before: '', after: 'exports.value = 2;\nexports.value = 2;\n', redacted: false, binary: false, tooLarge: false }));
  });
  await expect(page.getByLabel('Commit context')).toContainText('the commit changed');
  await page.getByRole('button', { name: 'Resend on the new version' }).click();
  await expect(page.getByRole('status').filter({ hasText: 'could not be found uniquely' })).toBeVisible();
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: unknown[] }).repairCalls.length)).toBe(0);
  await page.getByRole('button', { name: 'Take back', exact: true }).click();
  await expect(page.getByLabel('Commit context')).toHaveCount(0);
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Change this value.');
});

test('a held repair receives selected diff context in its existing session', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers(); const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  await page.getByRole('tab', { name: 'Check', exact: true }).click(); await page.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await page.getByRole('button', { name: 'Stop repair', exact: true }).click();
  await page.getByRole('tab', { name: 'Changes', exact: true }).click(); await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Keep the earlier fix and change this line.');
  await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls);
  expect(calls.filter(call => call.method === 'start')).toHaveLength(1);
  expect(calls.find(call => call.method === 'followup')?.request).toMatchObject({ id: '01a1a200-0000-5000-8000-000000000099', selection: { path: 'index.js', startLine: 1, side: 'after' } });
});

test('an unfinished saved-file restore holds repair controls and Refresh', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers(); const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  await page.getByRole('tab', { name: 'Check', exact: true }).click(); await page.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await page.getByRole('button', { name: 'Stop repair', exact: true }).click();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { commitState: { repairRuns: RepairRun[]; sequence: number } }).commitState;
    state.repairRuns[0].restoring = '01a1a200-0000-7000-8000-000000000099'; state.repairRuns[0].revision++; state.sequence++;
  });
  await expect(page.getByRole('region', { name: 'A · for the commit', exact: true })).toContainText('Restoring saved files');
  for (const name of ['Resume repair', 'Discard repair', 'Refresh']) await expect(page.getByRole('button', { name, exact: true })).toBeDisabled();
});

test('log lag identifies the repair and its Stop remains available', async () => {
  const job=fixture(); job.status='failed'; job.candidate!.state='check-failed';
  await mock(job); await repairHandlers(); const page=await app.firstWindow();
  await page.getByRole('button',{name:'Commit',exact:true}).click();
  await page.getByRole('tab',{name:'Check',exact:true}).click(); await page.getByRole('button',{name:'Ask Worker A to fix'}).click();
  await app.evaluate(({ipcMain}) => {
    const state=(globalThis as typeof globalThis & {commitState:{repairRuns:RepairRun[];outputFlows:unknown[];sequence:number}}).commitState;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => new Promise(() => {}));
    state.outputFlows=[{repairRunId:state.repairRuns[0].id,queuedBytes:5*1024*1024,oldestQueuedMs:300,droppedDeltas:100,interrupted:false,lagging:true,preview:{sessionId:state.repairRuns[0].session,turnId:'repair-turn',itemId:'repair-item',text:'Checked repair output: [redacted]',truncated:false}}]; state.sequence++;
  });
  await expect(page.getByRole('status').filter({hasText:'log lagging'})).toContainText('for the commit');
  await expect(page.getByRole('log',{name:'A · for the commit in progress',exact:true})).toContainText('Checked repair output: [redacted]');
  await app.evaluate(() => { const state=(globalThis as typeof globalThis & {commitState:{outputFlows:{oldestQueuedMs:number}[]}}).commitState; state.outputFlows[0].oldestQueuedMs=2500; });
  await expect(page.getByRole('status').filter({hasText:'New work paused'})).toContainText('Stop still works');
  await page.screenshot({ path: 'research/verify/output-flow-20261104/log-lagging.png' });
  await page.getByRole('button',{name:'Stop this repair',exact:true}).click();
  const calls=await app.evaluate(()=>(globalThis as typeof globalThis & {repairCalls:{method:string;request:{id:string}}[]}).repairCalls);
  expect(calls.find(call=>call.method==='stop')?.request.id).toBe('01a1a200-0000-5000-8000-000000000099');
});

test('a completed commit offers sending the retained draft as ordinary work', async () => {
  await mock(fixture()); await repairHandlers(); const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click(); await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Change this in the next task.');
  await app.evaluate(({ ipcMain }) => {
    const memory = globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; sequence: number }; stagedRequest: unknown };
    memory.commitState.integration.status = 'done'; memory.commitState.sequence++;
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => { memory.stagedRequest = request; return { id: 'saved-request', text: request.text, attachments: [], createdAt: Date.now(), state: 'staged', revision: 1, target: request.target, decision: null, reason: null }; });
  });
  await expect(page.getByLabel('Commit context')).toContainText('the commit is gone');
  await page.getByRole('button', { name: 'Send to A', exact: true }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const request = await app.evaluate(() => (globalThis as typeof globalThis & { stagedRequest: { text: string; target: unknown } }).stagedRequest);
  expect(request).toMatchObject({ text: 'Change this in the next task.', target: { mode: 'lock', trackId: fixture().tracks[0].id } });
});

test('Japanese selected-line context and its changed-version choices fit at 200 percent', async () => {
  await mock(fixture()); await repairHandlers(); const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click(); await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('この行の戻り値を三にしてください。既存の他の修正は残してください。');
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); const state = (globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; sequence: number } }).commitState; state.integration.revision++; state.sequence++; });
  await page.getByRole('button', { name: 'Hide commit review' }).click();
  const context = page.getByLabel('Commit context'); await expect(context).toContainText('the commit changed');
  const viewport = await page.evaluate(() => ({ height: innerHeight, width: innerWidth }));
  await writeFile('research/verify/repair-context-20261104/context-layout.json', JSON.stringify(await page.evaluate(() => Array.from(document.querySelectorAll('.app,.main,.main>*,.desk>*,.composer>*')).map(element => ({ name: element.className, height: element.getBoundingClientRect().height, top: element.getBoundingClientRect().top, flex: getComputedStyle(element).flex, minHeight: getComputedStyle(element).minHeight }))), null, 2));
  for (const label of ['Resend on the new version', 'Take back', 'Clear commit context']) {
    const box = (await context.getByRole('button', { name: label, exact: true }).boundingBox())!;
    expect(box.x).toBeGreaterThanOrEqual(0); expect(box.x + box.width).toBeLessThanOrEqual(viewport.width); expect(box.y + box.height).toBeLessThanOrEqual(viewport.height);
  }
  const send = (await page.getByRole('button', { name: /Send to repair/ }).boundingBox())!;
  expect(send.y + send.height).toBeLessThanOrEqual(viewport.height);
  await app.evaluate(async ({ BrowserWindow }, destination) => { const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage(); process.getBuiltinModule('fs').writeFileSync(destination, image.toPNG()); }, path.resolve('research/verify/repair-context-20261104/context-200.png'));
});

test('repair clips use a 26 pixel child row and the selected row owns the Stop shortcut', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; repairRuns: RepairRun[]; sequence: number; [key: string]: unknown } }).commitState;
    const now = Date.now(), track = state.integration.tracks[0].id;
    state.timeline = { summaries: [], clock: { epochMs: now, monoMs: 0, bootId: 'layout', timelineMs: now } };
    (globalThis as typeof globalThis & { commitClips: unknown[] }).commitClips = [
      { id: 'work-step', trackId: track, repairRunId: null, ordinal: 1, sessionId: 'work', turnId: 'work-turn', category: 'step', label: 'ordinary work', kind: 'edit', start: now - 240000, end: now - 120000, running: false, generationEnd: null, approximate: false, toolTicks: [] },
      { id: 'repair-step', trackId: track, repairRunId: state.repairRuns[0].id, ordinal: 2, sessionId: 'repair-session', turnId: 'repair-turn', category: 'step', label: 'repair work', kind: 'edit', start: now - 240000, end: now - 120000, running: false, generationEnd: null, approximate: false, toolTicks: [] },
    ];
    state.workers = [{ trackId: track, phase: 'working', sessionId: 'work', identity: null, detail: null, activeTurn: 'work-turn', output: null, checkpointPending: false, prompts: [], checkpoint: null, approvals: [] }];
    state.sequence++;
  });
  const row = page.locator('.repair-row'); await expect(row).toBeVisible();
  expect((await row.boundingBox())!.height).toBe(26);
  const regular = page.getByRole('button', { name: 'Open step A:1: ordinary work', exact: true });
  const repair = page.getByRole('button', { name: 'Open repair step A:2: repair work', exact: true });
  expect((await repair.boundingBox())!.y).toBeGreaterThan((await regular.boundingBox())!.y + 35);
  await repair.click(); await expect(page.locator('.inspector-heading')).toContainText('for the commit');
  await page.keyboard.press('Meta+.');
  await expect(sheet).toContainText('Repair held');
  await page.getByRole('gridcell', { name: 'A · Update the exported value · working', exact: true }).click();
  await page.keyboard.press('Meta+.');
  await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string }[] }).repairCalls.filter(call => call.method === 'work-stop').length)).toBe(1);
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls);
  expect(calls.find(call => call.method === 'stop')!.request.id).toBe('01a1a200-0000-5000-8000-000000000099');
  expect(calls.find(call => call.method === 'work-stop')!.request.trackId).toBe(job.tracks[0].id);
  await page.screenshot({ path: 'research/verify/repair-lanes-20261104/lanes.png' });
});

test('a held repair receives the composer instruction and retries the same operation', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers(false, true);
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await sheet.getByRole('button', { name: 'Stop repair', exact: true }).click();
  await expect(sheet).toContainText('Repair held');
  await page.locator('.repair-row').click();
  await expect(page.locator('.reply-target')).toContainText('for the commit');
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Keep the existing fix and cover the empty input.');
  await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(page.getByRole('button', { name: /Retry save/ })).toBeEnabled();
  await page.getByRole('button', { name: /Retry save/ }).click();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls.filter(call => call.method === 'followup'));
  expect(calls).toHaveLength(2);
  expect(calls[0].request).toEqual(calls[1].request);
  expect(calls[0].request).toMatchObject({ id: '01a1a200-0000-5000-8000-000000000099', text: 'Keep the existing fix and cover the empty input.' });
});

test('a rejected repair instruction stays editable without reusing its operation', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await sheet.getByRole('button', { name: 'Stop repair', exact: true }).click();
  await expect(sheet).toContainText('Repair held'); await page.locator('.repair-row').click();
  await app.evaluate(({ ipcMain }) => {
    const memory = globalThis as typeof globalThis & { commitState: { repairRuns: RepairRun[]; sequence: number }; repairCalls: { method: string; request: Record<string, unknown> }[] };
    let rejected = false;
    ipcMain.removeHandler('intercat:repair-followup'); ipcMain.handle('intercat:repair-followup', (_event, request) => {
      memory.repairCalls.push({ method: 'followup', request });
      if (!rejected) { rejected = true; return { status: 'rejected', reason: 'Remove the detected secret from this instruction before sending it.' }; }
      memory.commitState.repairRuns[0].phase = 'queued'; memory.commitState.sequence++;
      return { status: 'accepted', run: memory.commitState.repairRuns[0] };
    });
  });
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('Instruction requiring correction'); await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(draft).toBeEnabled(); await expect(draft).toHaveValue('Instruction requiring correction');
  await expect(page.locator('.toast')).toContainText('Remove the detected secret');
  await draft.fill('Corrected instruction'); await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(draft).toHaveValue('');
  const requests = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls.filter(call => call.method === 'followup').map(call => call.request));
  expect(requests[0].opId).not.toBe(requests[1].opId);
});

test('repair requests retry the same operation and Stop only the repair', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed'; job.candidate!.check!.code = 1;
  await mock(job); await repairHandlers(true);
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click();
  await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await expect(sheet).toContainText('Repair response lost after saving');
  await sheet.getByRole('button', { name: 'Retry repair request' }).click();
  const panel = sheet.getByRole('region', { name: 'A · for the commit', exact: true });
  await expect(panel).toContainText('Repairing');
  await expect(sheet.getByRole('button', { name: 'Refresh', exact: true })).toBeEnabled();
  await expect(sheet.getByRole('checkbox', { name: 'Include Worker A' })).toBeDisabled();
  await panel.getByRole('button', { name: 'Stop repair', exact: true }).click();
  await expect(panel).toContainText('Repair held');
  await panel.getByRole('button', { name: 'Resume repair' }).click();
  await expect(panel).toContainText('Waiting for a worker');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls);
  const start = calls.filter(call => call.method === 'start');
  expect(start).toHaveLength(2); expect(start[0].request.opId).toBe(start[1].request.opId);
  expect(calls.find(call => call.method === 'stop')!.request.id).toBe('01a1a200-0000-5000-8000-000000000099');
  expect(start[0].request.purpose).toEqual({ kind: 'check' });
  await panel.getByRole('button', { name: 'Discard repair' }).click();
  await expect(sheet.getByRole('button', { name: 'Refresh', exact: true })).toBeEnabled();
});

test('Refresh confirms stopping a repair and retries a lost acknowledgement once', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers(false, false, true);
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await sheet.getByRole('button', { name: 'Refresh', exact: true }).click();
  const confirm = sheet.getByRole('group', { name: 'Confirm Refresh' });
  await expect(confirm).toContainText('Refresh stops A’s repair');
  await confirm.getByRole('button', { name: 'Wait', exact: true }).click();
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string }[] }).repairCalls.filter(call => call.method === 'refresh').length)).toBe(0);
  await sheet.getByRole('button', { name: 'Refresh', exact: true }).click();
  await confirm.getByRole('button', { name: 'Refresh now' }).click();
  await expect(sheet).toContainText('Refresh response lost after saving');
  await sheet.getByRole('button', { name: 'Retry confirmed Refresh' }).click();
  await expect(sheet).toContainText('Checked and ready');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls.filter(call => call.method === 'refresh'));
  expect(calls).toHaveLength(2); expect(calls[0].request).toEqual(calls[1].request);
  await expect(sheet.getByRole('group', { name: 'Confirm Refresh' })).toHaveCount(0);
});

test('stale Refresh confirmation leaves the running repair alone', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await sheet.getByRole('button', { name: 'Refresh', exact: true }).click();
  await app.evaluate(() => { const state = (globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; sequence: number } }).commitState; state.integration.revision++; state.sequence++; });
  await sheet.getByRole('button', { name: 'Refresh now' }).click();
  await expect(sheet).toContainText('The review changed. Confirm Refresh again.');
  await expect(sheet.getByRole('region', { name: 'A · for the commit', exact: true })).toContainText('Repairing');
});

test('Refresh confirms dropping completed fixes and hides their old report', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed';
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { commitState: { integration: IntegrationJob; repairRuns: RepairRun[]; sequence: number } }).commitState;
    state.repairRuns[0].phase = 'done'; state.repairRuns[0].output = 'The candidate was repaired.';
    state.integration.status = 'checked'; state.integration.contributions = { fixed: {} }; state.sequence++;
  });
  await expect(sheet).toContainText('The candidate was repaired.');
  await sheet.getByRole('button', { name: 'Refresh', exact: true }).click();
  await expect(sheet.getByRole('group', { name: 'Confirm Refresh' })).toContainText('discards the reviewed fixes');
  await sheet.getByRole('button', { name: 'Refresh now' }).click();
  await expect(sheet).not.toContainText('The candidate was repaired.');
});

test('Refresh confirmation keeps its choices and footer in view at 200 percent', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed'; job.candidate!.check!.code = 1;
  await mock(job); await repairHandlers();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1440, 900); window.webContents.setZoomFactor(2); });
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click(); await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await sheet.getByRole('button', { name: 'Refresh', exact: true }).click();
  const viewport = await page.evaluate(() => ({ height: innerHeight, width: innerWidth }));
  for (const label of ['Refresh now', 'Wait', 'Commit']) {
    const button = sheet.getByRole('button', { name: label, exact: true });
    const box = await button.boundingBox();
    expect(box!.y + box!.height).toBeLessThanOrEqual(viewport.height);
    expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width);
  }
  await app.evaluate(async ({ BrowserWindow }, output) => { const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage(); process.getBuiltinModule('fs').writeFileSync(output, image.toPNG()); }, path.resolve('research/verify/repair-refresh-20261104/confirmation-200.png'));
});

test('repair approvals and questions keep their own target at 200 percent', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed'; job.candidate!.check!.code = 1;
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click();
  await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await app.evaluate(({ BrowserWindow }) => {
    BrowserWindow.getAllWindows()[0].setSize(1440, 900); BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2);
    const state = (globalThis as typeof globalThis & { commitState: { repairRuns: RepairRun[]; sequence: number } }).commitState;
    const run = state.repairRuns[0];
    run.approvals = [{ id: 'repair-approval', requirement: { sourceIndex: 2 }, subject: 'git push fixture', reason: { kind: 'push', message: 'Remote update needs approval' }, canAllow: true, canDeny: true }];
    run.prompts = [{ request: { sessionId: 'repair-session', userInputId: 'repair-question', turnId: 'repair-turn', questions: [{ id: 'choice', header: '修正する方法', question: '今回の修正をどの方法で進めますか？', selection: { mode: 'single' }, options: [{ label: '小さな修正', description: '既存の処理を保って修正する' }, { label: '作り直す' }] }] }, revision: 'f'.repeat(64), phase: 'open', operation: null }];
    state.sequence++;
  });
  const panel = sheet.getByRole('region', { name: 'A · for the commit', exact: true });
  await panel.getByRole('button', { name: 'Don’t run it' }).click();
  await panel.getByRole('radio', { name: '小さな修正' }).check();
  await panel.getByRole('button', { name: 'Send answers' }).click();
  await expect(panel.getByRole('button', { name: 'Stop repair', exact: true })).toBeVisible();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls);
  expect(calls.find(call => call.method === 'answer')!.request).toMatchObject({ id: '01a1a200-0000-5000-8000-000000000099', approvalId: 'repair-approval', requirement: { sourceIndex: 2 }, allow: false });
  expect(calls.find(call => call.method === 'respond')!.request).toMatchObject({ id: '01a1a200-0000-5000-8000-000000000099', promptId: 'repair-question', response: { action: 'answer', answers: [{ questionId: 'choice', selectedLabel: '小さな修正' }] } });
  const bounds = await panel.getByRole('button', { name: 'Stop repair', exact: true }).boundingBox();
  const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight, scrollWidth: document.documentElement.scrollWidth }));
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width);
  expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(viewport.height);
  expect(viewport.scrollWidth).toBe(viewport.width);
  await app.evaluate(async ({ BrowserWindow }, output) => {
    const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage();
    process.getBuiltinModule('fs').writeFileSync(output, image.toPNG());
  }, path.resolve('research/verify/repair-ui-20261104/repair-200.png'));
});

test('Remove starts a repair for the specific secret finding', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'secret-found'; job.candidate!.blockedCommit = job.candidate!.commits[0];
  job.candidate!.secretScans = [{ findings: [{ id: 'finding-to-remove', path: 'index.js', line: 1, endLine: 1, rule: 'github-pat' }] }];
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('button', { name: 'Ask to remove' }).click();
  await expect(sheet.getByRole('region', { name: 'A · for the commit', exact: true })).toBeVisible();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string; request: Record<string, unknown> }[] }).repairCalls);
  expect(calls[0].request.purpose).toEqual({ kind: 'removeSecret', finding: 'finding-to-remove' });
  await expect(sheet.getByRole('button', { name: 'Keep this finding' })).toBeDisabled();
});

test('Commit review stays nonmodal, edits messages and sends the reviewed candidate', async () => {
  await mock(fixture());
  const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await expect(sheet.getByRole('region', { name: 'Diff for index.js' })).toBeVisible();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('The main request is still editable.');
  await sheet.getByRole('tab', { name: 'Message', exact: true }).click();
  await sheet.getByRole('textbox', { name: 'Message for Worker A' }).fill('Reviewed subject');
  await expect(sheet.getByRole('button', { name: 'Commit', exact: true })).toBeDisabled();
  await sheet.getByRole('button', { name: 'Save message and check' }).click();
  await expect(sheet.getByRole('button', { name: 'Commit', exact: true })).toBeEnabled();
  const footer = await sheet.locator('.commit-footer').boundingBox();
  expect(footer!.y + footer!.height).toBeLessThanOrEqual(await page.evaluate(() => innerHeight));
  await page.screenshot({ path: 'research/verify/commit-ui-20261104/small-review.png' });
  await sheet.getByRole('button', { name: 'Commit', exact: true }).click();
  await expect(sheet).toContainText('Committed');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { commitCalls: { action: { action: string; revision: number; head: string } }[] }).commitCalls);
  const commit = calls.find(call => call.action.action === 'commit')!;
  expect(commit.action.revision).toBe(2); expect(commit.action.head).toBe('1'.repeat(40));
});

test('IME Escape leaves Commit review open and a later Escape closes it', async () => {
  await mock(fixture());
  const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await expect(sheet).toBeVisible();
  await sheet.evaluate(element => element.dispatchEvent(new KeyboardEvent('keydown', {
    key: 'Escape', bubbles: true, cancelable: true, isComposing: true,
  })));
  await expect(sheet).toBeVisible();
  await sheet.press('Escape');
  await expect(sheet).toHaveCount(0);
});

test('diff icons use the packaged icon font', async ({}, testInfo) => {
  await mock(fixture());
  const page=await app.firstWindow();
  await page.getByRole('button',{name:'Commit',exact:true}).click();
  const diff=page.getByRole('region',{name:'Diff for index.js',exact:true});
  await expect(diff.locator('.view-lines').last()).toBeVisible();
  await expect(diff.locator('.char-insert').first()).toBeVisible();
  const fonts=await page.evaluate(async()=>{
    await document.fonts.load('16px codicon');
    return Array.from(document.fonts).map(face=>({family:face.family,status:face.status}));
  });
  await page.screenshot({path:testInfo.outputPath('diff.png')});
  await testInfo.attach('font-faces',{body:JSON.stringify(fonts),contentType:'application/json'});
  expect(fonts.some(face=>face.family.replaceAll('"','')==='codicon' && face.status==='loaded')).toBe(true);
});

test('both windows keep Refresh disabled until a pending Commit finishes', async () => {
  const job=fixture(); job.status='committing';
  await mock(job);
  const first=await app.firstWindow();
  const opened=app.waitForEvent('window');
  await app.evaluate(async({BrowserWindow},preload)=>{
    const current=BrowserWindow.getAllWindows()[0];
    const window=new BrowserWindow({width:1040,height:760,webPreferences:{
      preload,
      contextIsolation:true,nodeIntegration:false,sandbox:true,
    }});
    await window.loadURL(current.webContents.getURL());
  },path.resolve('electron/preload.cjs'));
  const second=await opened;
  for(const page of [first,second]) {
    await page.getByRole('button',{name:'Commit',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Commit review'});
    await expect(sheet).toContainText('Committing… · wait');
    await expect(sheet.getByRole('button',{name:'Refresh',exact:true})).toBeDisabled();
  }
  await app.evaluate(()=>{
    const state=(globalThis as typeof globalThis & {commitState:{sequence:number;integration:IntegrationJob}}).commitState;
    state.integration.status='done';state.sequence++;
  });
  for(const page of [first,second]) await expect(page.getByRole('complementary',{name:'Commit review'})).toContainText('Committed ·');
});

test('a lost Keep response retries the same operation and never enables Commit prematurely', async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'secret-found'; job.candidate!.blockedCommit = job.candidate!.commits[0];
  job.candidate!.secretScans = [{ findings: [{ id: 'finding-one', path: 'index.js', line: 1, endLine: 1, rule: 'github-pat' }] }];
  await mock(job, true);
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('button', { name: 'Keep this finding' }).click();
  await expect(sheet).toContainText('Response lost after saving');
  await expect(sheet.getByRole('button', { name: 'Commit', exact: true })).toBeDisabled();
  await sheet.getByRole('button', { name: 'Retry action' }).click();
  await expect(sheet.getByRole('button', { name: 'Commit', exact: true })).toBeEnabled();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { commitCalls: { opId: string; action: { action: string } }[] }).commitCalls);
  const keep = calls.filter(call => call.action.action === 'keepSecret');
  expect(keep).toHaveLength(2); expect(keep[0].opId).toBe(keep[1].opId);
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { commitState: { sequence: number; integration: IntegrationJob; effects: unknown[] } }).commitState;
    state.integration.tracks[0].included = false; state.sequence++;
    state.effects = [{ id: 'effect', trackId: state.integration.tracks[0].id, sessionId: 's', itemId: 'i', turnId: 't', clipId: null, kind: 'push', target: 'example.invalid', ruleId: 'push', outcome: 'done', running: false, at: 1, reason: null }];
  });
  await expect(sheet.getByRole('button', { name: 'Commit', exact: true })).toBeDisabled();
  await sheet.locator('.commit-effects > summary').click();
  await expect(sheet.getByLabel('Effects outside Git')).toContainText('example.invalid');

});

test('Commit review stays usable at 200 percent and preserves a stale message draft', async () => {
  await mock(fixture());
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1440, 900); window.webContents.setZoomFactor(2); });
  const page = await app.firstWindow();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Message', exact: true }).click();
  const message = sheet.getByRole('textbox', { name: 'Message for Worker A' });
  await message.fill('Preserve my local draft.');
  await app.evaluate(() => { const value = (globalThis as typeof globalThis & { commitState: { sequence: number; integration: IntegrationJob } }).commitState; value.sequence++; value.integration.revision++; });
  await expect(sheet).toContainText('Your draft has been kept.');
  await expect(message).toHaveValue('Preserve my local draft.');
  await expect(sheet.getByRole('button', { name: 'Save message and check' })).toBeDisabled();
  await sheet.getByRole('button', { name: 'Discard message draft' }).click();
  const button = sheet.getByRole('button', { name: 'Commit', exact: true });
  await expect(button).toBeEnabled();
  const box = await button.boundingBox();
  const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight, dpr: devicePixelRatio, scrollWidth: document.documentElement.scrollWidth }));
  expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width);
  expect(box!.y + box!.height).toBeLessThanOrEqual(viewport.height);
  const composer = await page.getByRole('textbox', { name: 'Request draft' }).boundingBox();
  expect(composer!.y + composer!.height).toBeLessThanOrEqual(viewport.height);
  await message.focus(); await page.keyboard.press('Control+Tab');
  await expect(page.getByRole('region', { name: 'Stage', exact: true })).toBeFocused();
  // Electron's page screenshot clips zoomed content; capture the native view.
  await app.evaluate(async ({ BrowserWindow }, destination) => {
    const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage();
    process.getBuiltinModule('fs').writeFileSync(destination, image.toPNG());
  }, path.resolve('research/verify/commit-ui-20261104/zoom-review.png'));
  await writeFile('research/verify/commit-ui-20261104/zoom-layout.json', JSON.stringify({ viewport, button: box, composer }, null, 2));
  await sheet.getByRole('button', { name: 'Hide commit review' }).click();
  await expect(sheet).toHaveCount(0);
});

const dummyToken = 'ghp_' + 'A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H';
async function realFixture(secret = false, failedCheck = false, unborn = false) {
  const root = path.join(data, 'fixture');
  const seeded = JSON.parse(execFileSync('cargo', ['run', '--offline', '-q', '-p', 'intercat-daemon', '--example', 'commit_fixture', '--', root, ...(unborn ? ['--unborn'] : failedCheck ? ['--failed-check'] : [])], { encoding: 'utf8' }));
  execFileSync('cargo', ['build', '--offline', '-q', '--bin', 'intercatd']);
  if (secret) {
    const hook = path.join(seeded.source, '.git/hooks/commit-msg');
    await writeFile(hook, `#!/bin/sh\nprintf '\\n${dummyToken}\\n' >> \"$1\"\n`);
    await chmod(hook, 0o755);
  }
  const token = randomBytes(32).toString('hex');
  daemon = spawn(path.resolve('target/debug/intercatd'), ['serve', seeded.data, path.resolve('services/editor/src')], { stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env, INTERCAT_GITLEAKS: path.resolve('.tools/gitleaks-8.30.1/gitleaks') } });
  let startupError = '';
  daemon.stderr!.on('data', (chunk: Buffer) => { startupError = (startupError + chunk.toString()).slice(-4096); });
  const lines=createInterface({input:daemon.stdout!});
  const handshake=(async()=>{
    for await (const line of lines) { const hello=JSON.parse(line); if(!hello.fingerprints) throw new Error('Invalid fixture fingerprint handshake');daemon!.stdin!.end(JSON.stringify(hello.fingerprints.catalog)+'\n');lines.close();return; }
    throw new Error(`Fixture bootstrap failed: ${startupError}`);
  })();
  daemon.stdin!.write(JSON.stringify({ token, secretKey: Buffer.alloc(32, 7).toString('hex'), receipt:null }) + '\n');
  await handshake;
  await expect.poll(async () => {
    if (daemon!.exitCode !== null) throw new Error(`The fixture daemon exited (${daemon!.exitCode}): ${startupError}`);
    try { execFileSync('/bin/test', ['-S', path.join(seeded.data, 'run/rpc.sock')]); return true; } catch { return false; }
  }, { timeout: 15000 }).toBe(true);
  await app.evaluate(async ({ ipcMain, BrowserWindow }, input) => {
    const load = process.getBuiltinModule('module').createRequire(input.module);
    const { RpcClient } = load(input.module);
    const client = await RpcClient.connect(input.socket, input.token);
    const project = (await client.request('system.status')).projectSecrets;
    if (project) {
      const { createHmac } = load('node:crypto');
      const operation = load(input.daemon).operationId();
      const keyCheck = createHmac('sha256', Buffer.alloc(32, 7)).update('intercat.secret-fingerprints.v1').digest('hex');
      const catalog = await client.request('secret.project.prepare', { opId: operation, projectId: project.projectId, revision: 0, fingerprints: [], keyCheck });
      await client.request('secret.confirm', catalog.current);
      await client.request('secret.project.activate', { projectId: project.projectId, revision: 0, entries: [] });
    }
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', async () => ({ connected: true, ...await client.request('system.status') }));
    ipcMain.removeHandler('intercat:output-flows'); ipcMain.handle('intercat:output-flows', () => client.request('system.outputFlows', {}));
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', (_event, request) => client.request('timeline.window', request));
    ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request) => client.request('integration.command', request));
    ipcMain.removeHandler('intercat:integration-diff'); ipcMain.handle('intercat:integration-diff', (_event, request) => client.request('integration.diff', request));
    ipcMain.removeHandler('intercat:clip-detail'); ipcMain.handle('intercat:clip-detail', (_event, clipId) => client.request('timeline.detail', { clipId }));
    ipcMain.removeHandler('intercat:restore-deletion'); ipcMain.handle('intercat:restore-deletion', (_event, request) => client.request('deletion.restore', request));
    for (const method of ['start', 'stop', 'control', 'answer', 'respond', 'followup']) {
      ipcMain.removeHandler(`intercat:repair-${method}`); ipcMain.handle(`intercat:repair-${method}`, (_event, request) => client.request(`repair.${method}`, request));
    }
    (globalThis as typeof globalThis & { repairFixtureClient: typeof client }).repairFixtureClient = client;
    BrowserWindow.getAllWindows()[0].setSize(1440, 900);
  }, { module: path.resolve('electron/rpc-client.mjs'), daemon: path.resolve('electron/daemon.mjs'), socket: path.join(seeded.data, 'run/rpc.sock'), token });
  return seeded as { source: string; data: string };
}

test('desktop sends selected checked lines to a real isolated repair', async () => {
  test.skip(process.env.INTERCAT_TEST_CONTEXT_UI !== '1', 'Opt-in paid dummy selected-line repair');
  test.setTimeout(180000);
  const seeded = await realFixture();
  const output = path.resolve('research/verify/repair-context-20261104/native'); await mkdir(output, { recursive: true });
  evidenceDatabase = { source: path.join(seeded.data, 'events.sqlite'), target: path.join(output, 'events.sqlite') };
  const page = await app.firstWindow();
  const opId = await page.evaluate(() => window.intercat.createOperationId());
  await app.evaluate(async (_electron, input) => {
    const client = (globalThis as typeof globalThis & { repairFixtureClient: { request: (method: string, params: unknown) => Promise<unknown> } }).repairFixtureClient;
    await client.request('worker.configure', { opId: input.opId, source: input.binary, credentials: input.auth });
  }, { opId, binary: process.env.INTERCAT_TEST_MUSE_BINARY!, auth: process.env.INTERCAT_TEST_MUSE_AUTH! });
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' }); await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
  await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Set the exported value on this selected line to exactly 3. Edit index.js with the file editing tool. Preserve unrelated files and do not make Git commits. Finish with a brief final report.');
  await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(sheet.getByRole('region', { name: 'A · for the commit', exact: true })).toBeVisible();
  await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).repairRuns?.[0]?.phase, { timeout: 120000 }).toBe('done');
  const final = await page.evaluate(() => window.intercat.getStatus());
  expect(final.integration!.status).toBe('checked');
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', `${final.integration!.candidate!.head}:index.js`], { encoding: 'utf8' })).toBe('exports.value = 3;\n');
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', 'HEAD:index.js'], { encoding: 'utf8' })).toBe('exports.value = 1;\n');
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'for-each-ref', '--format=%(refname)', 'refs/heads/intercat/'], { encoding: 'utf8' }).trim()).toBe('');
  await page.screenshot({ path: path.join(output, 'completed.png') });
  await writeFile(path.join(output, 'result.json'), JSON.stringify({ final, originalUnchanged: true }, null, 2));
});

test('desktop restores a deletion to its real held repair and keeps ordinary work unchanged', async () => {
  test.skip(process.env.INTERCAT_TEST_REPAIR_DELETE_UI !== '1', 'Opt-in paid dummy repair deletion');
  test.setTimeout(200000);
  const seeded = await realFixture();
  const output = path.resolve('research/verify/repair-deletion-20261104/native'); await mkdir(output, { recursive: true });
  evidenceDatabase = { source: path.join(seeded.data, 'events.sqlite'), target: path.join(output, 'events.sqlite') };
  const page = await app.firstWindow(); const opId = await page.evaluate(() => window.intercat.createOperationId());
  await app.evaluate(async (_electron, input) => {
    const client = (globalThis as typeof globalThis & { repairFixtureClient: { request: (method: string, params: unknown) => Promise<unknown> } }).repairFixtureClient;
    await client.request('worker.configure', { opId: input.opId, source: input.binary, credentials: input.auth });
  }, { opId, binary: process.env.INTERCAT_TEST_MUSE_BINARY!, auth: process.env.INTERCAT_TEST_MUSE_AUTH! });
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' }); await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
  await selectDiffLine(page);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('This disposable task tests deletion recovery. Create fleeting.txt containing exactly snapshot-value followed by a newline with the file editing tool. Then use the bash tool to run exactly rm -- fleeting.txt once. Do not delete in another way. Keep index.js and other files unchanged, do not make Git commits. Do not ask an interactive question. End your final report with this exact standalone line: Waiting for you: inspect the saved deletion.');
  await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).repairRuns?.[0]?.phase, { timeout: 120000 }).toBe('held');
  const state = await page.evaluate(() => window.intercat.getStatus()); const run = state.repairRuns![0];
  const workspace = path.join(seeded.data, 'integration', run.jobId, `repair-${run.id}`, 'repo');
  await expect(readFile(path.join(workspace, 'fleeting.txt'))).rejects.toThrow();
  const details = await page.evaluate(async clips => Promise.all(clips.map(clip => window.intercat.getClipDetail(clip.id))), (await timelineClips(page)).filter(clip => clip.repairRunId === run.id));
  const detail = details.find(detail => detail.deletionSnapshots?.some(snapshot => snapshot.repairRunId === run.id && snapshot.paths.includes('fleeting.txt')))!;
  expect(detail).toBeTruthy();
  const saved = detail.deletionSnapshots!.find(snapshot => snapshot.purpose === 'before-delete')!;
  expect(saved.repairRunId).toBe(run.id); expect(saved.complete).toBe(true);
  const head = execFileSync('/usr/bin/git', ['-C', workspace, 'rev-parse', 'HEAD'], { encoding: 'utf8' });
  await page.getByRole('button', { name: 'Hide commit review' }).click();
  const step = page.getByRole('button', { name: new RegExp(`Open repair step A:${detail.clip.ordinal}:`) }); await step.focus(); await step.press('Enter');
  const recovery = page.getByRole('region', { name: 'Before deletion', exact: true });
  await recovery.getByRole('button', { name: 'Restore saved files', exact: true }).click();
  await recovery.getByRole('button', { name: 'Restore now', exact: true }).click();
  await expect(recovery).toContainText('The repair is held.', { timeout: 30000 });
  expect(await readFile(path.join(workspace, 'fleeting.txt'), 'utf8')).toBe('snapshot-value\n');
  expect(execFileSync('/usr/bin/git', ['-C', workspace, 'rev-parse', 'HEAD'], { encoding: 'utf8' })).toBe(head);
  await expect(readFile(path.join(seeded.source, 'fleeting.txt'))).rejects.toThrow();
  for (const ordinary of state.workspaces ?? []) await expect(readFile(path.join(ordinary.path, 'fleeting.txt'))).rejects.toThrow();
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', 'HEAD:index.js'], { encoding: 'utf8' })).toBe('exports.value = 1;\n');
  await page.screenshot({ path: path.join(output, 'restored.png') });
  await writeFile(path.join(output, 'result.json'), JSON.stringify({ saved, run: (await page.evaluate(() => window.intercat.getStatus())).repairRuns![0], originalUnchanged: true }, null, 2));
});

test('desktop follows up a stopped repair through its own lane and composer', async () => {
  test.skip(process.env.INTERCAT_TEST_REPAIR_FOLLOWUP_UI !== '1', 'Opt-in paid dummy repair turns');
  test.setTimeout(200000);
  const seeded = await realFixture(false, true);
  const output = path.resolve('research/verify/repair-lanes-20261104/followup-ui'); await mkdir(output, { recursive: true });
  evidenceDatabase = { source: path.join(seeded.data, 'events.sqlite'), target: path.join(output, 'events.sqlite') };
  const page = await app.firstWindow();
  const opId = await page.evaluate(() => window.intercat.createOperationId());
  await app.evaluate(async (_electron, input) => {
    const client = (globalThis as typeof globalThis & { repairFixtureClient: { request: (method: string, params: unknown) => Promise<unknown> } }).repairFixtureClient;
    await client.request('worker.configure', { opId: input.opId, source: input.binary, credentials: input.auth });
  }, { opId, binary: process.env.INTERCAT_TEST_MUSE_BINARY!, auth: process.env.INTERCAT_TEST_MUSE_AUTH! });
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await expect(sheet).toContainText('Check failed', { timeout: 30000 });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click();
  await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).repairRuns?.[0]?.phase, { timeout: 30000 }).toBe('running');
  const before = (await page.evaluate(() => window.intercat.getStatus())).repairRuns![0];
  await sheet.getByRole('button', { name: 'Stop repair', exact: true }).click();
  await expect(sheet).toContainText('Repair held', { timeout: 30000 });
  await page.locator(`#repair-${before.id}`).click();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Continue fixing the syntax error in index.js. Preserve README.md. Use the file editing tools and finish with a brief report.');
  await page.getByRole('button', { name: /Send to repair/ }).click();
  await expect(sheet).toContainText('Checked and ready', { timeout: 120000 });
  const after = await page.evaluate(() => window.intercat.getStatus());
  expect(after.repairRuns![0].session).toBe(before.session);
  expect(after.repairRuns![0].phase).toBe('done');
  expect(before.session).toBeTruthy();
  const repairClips = (await timelineClips(page)).filter(clip => clip.sessionId === before.session);
  expect(repairClips.length).toBeGreaterThan(0);
  expect(repairClips.every(clip => clip.repairRunId === before.id)).toBe(true);
  expect((await page.locator(`#repair-${before.id}`).boundingBox())!.height).toBe(26);
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', 'HEAD:index.js'], { encoding: 'utf8' })).toBe('exports.value = 1;\n');
  await page.screenshot({ path: path.join(output, 'completed.png') });
  await writeFile(path.join(output, 'result.json'), JSON.stringify({ before, after, originalUnchanged: true }, null, 2));
});

test('desktop repairs a real failing Check and preserves the fix when editing its message', async () => {
  test.skip(process.env.INTERCAT_TEST_REPAIR_UI !== '1', 'Opt-in paid dummy repair turn');
  test.setTimeout(180000);
  const seeded = await realFixture(false, true);
  const output = path.resolve('research/verify/repair-ui-20261104/real'); await mkdir(output, { recursive: true });
  const opId = await (await app.firstWindow()).evaluate(() => window.intercat.createOperationId());
  await app.evaluate(async (_electron, input) => {
    const client = (globalThis as typeof globalThis & { repairFixtureClient: { request: (method: string, params: unknown) => Promise<unknown> } }).repairFixtureClient;
    await client.request('worker.configure', { opId: input.opId, source: input.binary, credentials: input.auth });
  }, { opId, binary: process.env.INTERCAT_TEST_MUSE_BINARY!, auth: process.env.INTERCAT_TEST_MUSE_AUTH! });
  try {
    const page = await app.firstWindow();
    await expect(page.getByRole('button', { name: 'Commit', exact: true })).toBeEnabled();
    await page.getByRole('button', { name: 'Commit', exact: true }).click();
    const sheet = page.getByRole('complementary', { name: 'Commit review' });
    await expect(sheet).toContainText('Check failed', { timeout: 30000 });
    await sheet.getByRole('tab', { name: 'Check', exact: true }).click();
    await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
    await expect(sheet.getByRole('region', { name: 'A · for the commit', exact: true })).toBeVisible();
    await expect(sheet).toContainText('Checked and ready', { timeout: 120000 });
    const repaired = (await page.evaluate(async () => window.intercat.getStatus())).integration!;
    const fixedTree = repaired.candidate!.tree;
    await page.screenshot({ path: path.join(output, 'repaired.png') });
    await sheet.getByRole('tab', { name: 'Message', exact: true }).click();
    await sheet.getByRole('textbox', { name: 'Message for Worker A' }).fill('Preserve the syntax repair');
    await sheet.getByRole('button', { name: 'Save message and check' }).click();
    await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
    const changed = (await page.evaluate(async () => window.intercat.getStatus())).integration!;
    expect(changed.candidate!.tree).toBe(fixedTree);
    expect(changed.candidate!.head).not.toBe(repaired.candidate!.head);
    expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', 'HEAD:index.js'], { encoding: 'utf8' })).toBe('exports.value = 1;\n');
    expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'for-each-ref', '--format=%(refname)', 'refs/heads/intercat/'], { encoding: 'utf8' }).trim()).toBe('');
    await writeFile(path.join(output, 'result.json'), JSON.stringify({ repaired, changed, originalUnchanged: true }, null, 2));
  } finally {
    // Take a consistent copy even if an unexpected prompt blocks the fixture.
    execFileSync('python3', ['-c', 'import sqlite3,sys; source=sqlite3.connect(sys.argv[1]); target=sqlite3.connect(sys.argv[2]); source.backup(target); target.close(); source.close()', path.join(seeded.data, 'events.sqlite'), path.join(output, 'events.sqlite')]);
  }
});

test('desktop commits real saved work through the authenticated daemon', async () => {
  test.setTimeout(90000);
  const seeded = await realFixture();
  const page = await app.firstWindow();
  const mainHead = execFileSync('/usr/bin/git', ['-C', seeded.source, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  await expect(page.getByRole('button', { name: 'Commit', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
  await expect(sheet.getByRole('region', { name: 'Diff for index.js' })).toBeVisible();
  const beforeClose = await page.evaluate(async () => (await window.intercat.getStatus()).integration);
  await sheet.getByRole('button', { name: 'Hide commit review' }).click();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
  const afterReopen = await page.evaluate(async () => (await window.intercat.getStatus()).integration);
  expect(afterReopen?.id).toBe(beforeClose?.id);
  expect(afterReopen?.candidate?.head).toBe(beforeClose?.candidate?.head);
  await page.screenshot({ path: 'research/verify/commit-ui-20261104/real-review.png' });
  await sheet.getByRole('tab', { name: 'Message', exact: true }).click();
  await sheet.getByRole('textbox', { name: 'Message for Worker A' }).fill('Verified desktop commit');
  await sheet.getByRole('button', { name: 'Save message and check' }).click();
  await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
  await sheet.getByRole('button', { name: 'Commit', exact: true }).click();
  await expect(sheet).toContainText('Committed', { timeout: 15000 });
  await page.screenshot({ path: 'research/verify/commit-ui-20261104/real-committed.png' });
  const refs = execFileSync('/usr/bin/git', ['-C', seeded.source, 'for-each-ref', '--format=%(refname)', 'refs/heads/intercat/'], { encoding: 'utf8' }).trim();
  expect(refs).toMatch(/^refs\/heads\/intercat\//);
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', `${refs}:index.js`], { encoding: 'utf8' })).toContain('exports.value = 2;');
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'show', `${refs}:README.md`], { encoding: 'utf8' })).toContain('now 2');
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim()).toBe(mainHead);
});

test('desktop commits an unborn project while main stays empty and Open PR explains why it is disabled', async () => {
  test.setTimeout(90000);
  const output = path.resolve('research/verify/unborn-project-20261104');
  await mkdir(output, { recursive: true });
  const seeded = await realFixture(false, false, true);
  evidenceDatabase = { source: path.join(seeded.data, 'events.sqlite'), target: path.join(output, 'events.sqlite') };
  const head = await readFile(path.join(seeded.source, '.git/HEAD'), 'utf8');
  await expect(readFile(path.join(seeded.source, '.git/index'))).rejects.toMatchObject({ code: 'ENOENT' });
  const page = await app.firstWindow();
  await expect(page.getByRole('button', { name: 'Commit', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await expect.poll(async () => {
    const state = await page.evaluate(async () => window.intercat.getStatus());
    await writeFile(path.join(output, 'latest-state.json'), JSON.stringify(state, null, 2));
    return state.integration?.status;
  }, { timeout: 30000 }).toBe('checked');
  const reviewed = (await page.evaluate(async () => window.intercat.getStatus())).integration!;
  expect(reviewed.project.baseBranch).toBeNull();
  expect(reviewed.candidate!.check!.code).toBe(0);
  await sheet.getByRole('button', { name: 'Commit', exact: true }).click();
  await expect(sheet).toContainText('Committed', { timeout: 15000 });
  await app.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    window.setSize(1040, 760); window.webContents.setZoomFactor(2);
  });
  const openPR = sheet.getByRole('button', { name: 'Open PR', exact: true });
  await expect(openPR).toBeVisible(); await expect(openPR).toBeDisabled();
  await expect(openPR).toHaveAttribute('title', 'no base branch yet');
  await expect(sheet).toContainText('no base branch yet');
  await expect(sheet.getByRole('button', { name: 'Done', exact: true })).toBeInViewport();
  await app.evaluate(async ({ BrowserWindow }, destination) => {
    const capture = await BrowserWindow.getAllWindows()[0].webContents.capturePage();
    process.getBuiltinModule('fs').writeFileSync(destination, capture.toPNG());
  }, path.join(output, 'committed-200.png'));
  const git = (args: string[]) => execFileSync('/usr/bin/git', ['-C', seeded.source, ...args], { encoding: 'utf8' }).trim();
  const branch = git(['for-each-ref', '--format=%(refname)', 'refs/heads/intercat/']);
  const base = reviewed.project.base;
  expect(git(['for-each-ref', '--format=%(refname)', 'refs/heads/main'])).toBe('');
  expect(await readFile(path.join(seeded.source, '.git/HEAD'), 'utf8')).toBe(head);
  await expect(readFile(path.join(seeded.source, '.git/index'))).rejects.toMatchObject({ code: 'ENOENT' });
  expect(git(['status', '--porcelain'])).toBe('');
  expect(git(['rev-list', '--parents', '-n', '1', base])).toBe(base);
  expect(git(['ls-tree', base])).toBe('');
  expect(git(['rev-list', '--count', branch])).toBe('3');
  expect(git(['show', `${branch}:index.js`])).toBe('exports.value = 2;');
  expect(git(['show', `${branch}:README.md`])).toContain('now 2');
  const completed = (await page.evaluate(async () => window.intercat.getStatus())).integration!;
  await writeFile(path.join(output, 'integration.json'), JSON.stringify({ reviewed, completed, head, branch, paidTurns: 0, mainEmpty: true, sourceUnchanged: true, indexAbsent: true, openPRDisabled: true }, null, 2));
  git(['bundle', 'create', path.join(output, 'source.bundle'), branch, `refs/intercat/starting-points/${base}`]);
  evidenceDatabase = { source: path.join(seeded.data, 'events.sqlite'), target: path.join(output, 'events.sqlite') };
});


test('desktop Keep resumes both exact commits and hides the detected values', async () => {
  test.setTimeout(90000);
  const seeded = await realFixture(true);
  const page = await app.firstWindow();
  await expect(page.getByRole('button', { name: 'Commit', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await expect(sheet.getByLabel('Secret findings')).toBeVisible({ timeout: 30000 });
  await sheet.getByRole('button', { name: /^Commit message:/ }).click();
  await sheet.getByText('Final message after hooks', { exact: true }).click();
  await expect(sheet.locator('.commit-message pre')).toContainText('[redacted]');
  await page.screenshot({ path: 'research/verify/commit-ui-20261104/real-secret-found.png' });
  for (const count of [1, 2]) {
    await sheet.getByRole('button', { name: 'Keep this finding', exact: true }).click();
    await expect(sheet).toContainText(`${count} secret finding${count === 1 ? '' : 's'} kept in this commit.`, { timeout: 30000 });
  }
  await expect(sheet).toContainText('Checked and ready', { timeout: 30000 });
  await expect(sheet).not.toContainText(dummyToken);
  await sheet.getByRole('button', { name: 'Commit', exact: true }).click();
  await expect(sheet).toContainText('Committed', { timeout: 15000 });
  await page.screenshot({ path: 'research/verify/commit-ui-20261104/real-kept-committed.png' });
  const branch = execFileSync('/usr/bin/git', ['-C', seeded.source, 'for-each-ref', '--format=%(refname)', 'refs/heads/intercat/'], { encoding: 'utf8' }).trim();
  expect(execFileSync('/usr/bin/git', ['-C', seeded.source, 'log', '--format=%B', '-2', branch], { encoding: 'utf8' })).toContain(dummyToken);
});

for (const kind of ['report', 'msp'] as const) test(`${kind} repair question uses a private secret form addressed to its repair Run`, async () => {
  const job = fixture(); job.status = 'failed'; job.candidate!.state = 'check-failed'; job.candidate!.check!.code = 1;
  await mock(job); await repairHandlers();
  const page = await app.firstWindow(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review' });
  await sheet.getByRole('tab', { name: 'Check', exact: true }).click();
  await sheet.getByRole('button', { name: 'Ask Worker A to fix' }).click();
  await app.evaluate(({ ipcMain, BrowserWindow }, kind) => {
    const memory = globalThis as typeof globalThis & { commitState: { repairRuns: RepairRun[]; sequence: number }; secretRequest?: unknown };
    const run = memory.commitState.repairRuns[0];
    run.phase = kind === 'msp' ? 'running' : 'held'; run.revision = 7; run.output = 'Waiting for you: Provide DEMO_TOKEN.';
    if (kind === 'msp') run.prompts = [{ request: { sessionId: 'repair-session', userInputId: 'repair-prompt', turnId: run.turn!, questions: [{ id: 'token', header: 'Token', question: 'Provide DEMO_TOKEN', selection: { mode: 'single' }, options: [{ label: 'Use token' }, { label: 'Stop' }] }] }, revision: 'a'.repeat(64), phase: 'open', operation: null }];
    memory.commitState.sequence++;
    ipcMain.removeHandler('intercat:worker-secret'); ipcMain.handle('intercat:worker-secret', (_event, request) => {
      memory.secretRequest = request; run.phase = 'queued'; run.secretInput = request.opId; memory.commitState.sequence++;
      return { accepted: true };
    });
    BrowserWindow.getAllWindows()[0].setSize(1440, 1000);
    BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2);
  }, kind);
  const panel = sheet.getByRole('region', { name: 'A · for the commit', exact: true });
  if (kind === 'msp') {
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { commitState: { repairRuns: RepairRun[]; sequence: number; secretInputs: unknown[] } }).commitState;
      state.secretInputs = [{ owner: { kind: 'repair', runId: state.repairRuns[0].id }, opId: 'pending', inputId: 'pending', trackId: state.repairRuns[0].track, name: 'DEMO_TOKEN', state: 'stopping', detail: null }];
      state.sequence++;
    });
    await expect(panel.getByRole('button', { name: 'Provide a secret', exact: true })).toBeDisabled();
    await expect(panel.getByRole('group', { name: 'Questions for A · for the commit' }).getByRole('button', { name: 'Stop', exact: true })).toBeEnabled();
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { commitState: { sequence: number; secretInputs: unknown[] } }).commitState;
      state.secretInputs = []; state.sequence++;
    });
  }
  await panel.getByRole('button', { name: kind === 'msp' ? 'Provide a secret' : 'Provide secret', exact: true }).click();
  await expect(panel.getByLabel('Save for this project')).toBeChecked();
  await expect(panel.getByLabel('Secret value')).toHaveAttribute('type', 'password');
  await panel.getByLabel('This time only', { exact: true }).check();
  await panel.getByLabel('Secret variable name').fill('DEMO_TOKEN');
  await panel.getByLabel('Secret value').fill('dummy-repair-ui-value');
  await panel.getByRole('button', { name: 'Use for this worker' }).click();
  await expect(panel.getByLabel('Secret value')).toHaveCount(0);
  const request = await app.evaluate(() => (globalThis as typeof globalThis & { secretRequest: unknown }).secretRequest);
  expect(request).toMatchObject({ owner: { kind: 'repair', runId: '01a1a200-0000-5000-8000-000000000099' }, trackId: job.tracks[0].id, sessionId: 'repair-session', turnId: '01a1a200-0000-7000-8000-000000000077', question: kind === 'msp' ? { kind: 'msp', id: 'repair-prompt', revision: 'a'.repeat(64) } : { kind: 'report', id: '01a1a200-0000-5000-8000-000000000099', revision: 7 }, scope: 'once', name: 'DEMO_TOKEN', value: 'dummy-repair-ui-value' });
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { repairCalls: { method: string }[] }).repairCalls);
  expect(calls.map(call => call.method)).toEqual(['start']);
});
