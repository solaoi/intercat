import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp(path.join(tmpdir(), 'intercat-desktop-'));
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${data}`], env: { ...process.env, ELECTRON_DISABLE_SECURITY_WARNINGS: 'false' } });
});
test.afterEach(async () => { if (app) await closeWindow(app); await rm(data, { recursive: true, force: true }); });

test('reference file chips survive a lost save acknowledgement and use the same original IDs', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const attachment = { id: '01a1a200-0000-7000-8000-000000000099', name: '参考資料-brief.txt', byteLen: 18, sha256: 'a'.repeat(64) };
    const state = { connected: true, sequence: 1, workerConfigured: true, tracks: [], workers: [], utterances: [] as unknown[], editor: { phase: 'ready' } };
    const calls: { opId: string; text: string; attachmentIds: string[]; target: object }[] = [];
    (globalThis as typeof globalThis & { attachmentCalls: typeof calls }).attachmentCalls = calls;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:attach-reference'); ipcMain.handle('intercat:attach-reference', () => attachment);
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => {
      calls.push(request);
      if (calls.length === 1) throw new Error('acknowledgement lost');
      const word = { id: request.opId, text: request.text, attachments: [attachment], createdAt: Date.now(), state: 'staged', revision: 1, target: request.target, decision: null, reason: null };
      state.utterances = [word]; state.sequence++; return word;
    });
    BrowserWindow.getAllWindows()[0].setSize(1440, 1000);
    BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2);
  });
  await page.getByRole('button', { name: 'Attach reference file', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Remove reference 参考資料-brief.txt', exact: true })).toBeVisible();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Explain the reference.');
  await page.locator('.composer').scrollIntoViewIfNeeded();
  await expect(page.getByRole('button', { name: 'Remove reference 参考資料-brief.txt', exact: true })).toBeInViewport();
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/attachment-originals-20261104/reference-composer.png', Buffer.from(pixels, 'base64'));
  await page.getByRole('button', { name: 'Put', exact: false }).click();
  await expect(page.getByRole('button', { name: 'Retry save', exact: false })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Remove reference 参考資料-brief.txt', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Retry save', exact: false }).click();
  await expect(page.getByLabel('Attached reference files')).toContainText('参考資料-brief.txt');
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { attachmentCalls: unknown[] }).attachmentCalls);
  expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
  expect(calls[0]).toMatchObject({ attachmentIds: ['01a1a200-0000-7000-8000-000000000099'], text: 'Explain the reference.' });
  const savedPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/attachment-originals-20261104/reference-ui.png', Buffer.from(savedPixels, 'base64'));
});

test('fingerprint rollback identifies affected sessions without reopening history', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: false, mode: 'unavailable', message: 'Restore the latest database, then reconnect.', fingerprintRecovery: { sessions: [{ sessionId: 'affected-session', trackId: 'affected-track', reason: 'missing' }], projects: [] } }));
    BrowserWindow.getAllWindows()[0].reload();
  });
  const review = page.getByRole('region', { name: 'Fingerprint recovery review' });
  await expect(review).toContainText('Values may appear unmasked');
  await expect(review).toContainText('affected-session');
  await expect(review).toContainText('affected-track');
  await expect(review).toContainText('fingerprints missing from this database');
  await expect(page.getByRole('button', { name: 'Reconnect', exact: true })).toBeVisible();
  await expect(page.locator('.worker-output')).toHaveCount(0);
});

test('fingerprint reconstruction keeps its operation on retry and leaves history closed', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const projectId = 'a'.repeat(64);
    const requests: { opId: string; projectId: string }[] = [];
    const memory = globalThis as typeof globalThis & { fingerprintRequests: typeof requests };
    memory.fingerprintRequests = requests;
    const before = { connected: false, mode: 'unavailable', message: 'History remains paused.', fingerprintRecovery: { projects: [{ projectId, reason: 'changed' }], sessions: [] } };
    let state: object = before;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:rebuild-fingerprints');
    ipcMain.handle('intercat:rebuild-fingerprints', (_event, request) => {
      requests.push(request);
      if (requests.length === 1) throw new Error('acknowledgement unavailable');
      state = { ...before, fingerprintRecovery: { projects: [], sessions: [], rebuiltProjects: [projectId], historyReviewRequired: true } };
      return state;
    });
    BrowserWindow.getAllWindows()[0].reload();
  });
  const review = page.getByRole('region', { name: 'Fingerprint recovery review' });
  await review.getByRole('button', { name: 'Rebuild saved fingerprints' }).click();
  await expect(review.getByRole('alert')).toContainText('acknowledgement unavailable');
  await review.getByRole('button', { name: 'Rebuild saved fingerprints' }).click();
  await expect(review).toContainText('Saved fingerprints rebuilt for 1 project');
  await expect(review).toContainText('File and session review is still required');
  await expect(review).toBeFocused();
  const requests = await app.evaluate(() => (globalThis as typeof globalThis & { fingerprintRequests: { opId: string; projectId: string }[] }).fingerprintRequests);
  expect(requests).toHaveLength(2); expect(requests[1]).toEqual(requests[0]);
  await expect(page.locator('.status-bar')).toContainText('Daemon not connected');
});

test('old input markers use the time window, retain close inputs and apply deltas', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const now = Date.now();
    const state = { connected: true, daemonPid: 7, sequence: 1, tracks: [{ id: 'a', label: 'A', title: 'First work', status: 'done' }, { id: 'b', label: 'B', title: 'Second work', status: 'done' }], utterances: [], workers: [], timeline: { clock: { epochMs: now, monoMs: 1000, bootId: 'marks', timelineMs: now }, summaries: [] } };
    const marks = [
      { id: 'session/old', trackId: 'a', repairRunId: null, at: now - 280000, inputId: 'old', text: 'Old input beyond the former 200 item limit' },
      { id: 'session/close', trackId: 'b', repairRunId: null, at: now - 279000, inputId: 'close', text: 'Closely spaced input with its full text' },
      { id: 'session/remove', trackId: 'a', repairRunId: null, at: now - 60000, inputId: 'remove', text: 'Removed on the next revision' },
    ];
    const memory = globalThis as typeof globalThis & { advanceMarks: () => void };
    memory.advanceMarks = () => { state.sequence = 2; };
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', (_event, request) => ({ revision: state.sequence, reset: request.since === null, clips: [], removed: [], marks: request.since === state.sequence ? [] : (state.sequence === 1 ? marks : [{ ...marks[0], text: 'Updated old input' }, marks[1]]).filter(mark => mark.at >= request.start && mark.at <= request.end), removedMarks: request.since === 1 && state.sequence === 2 ? ['session/remove'] : [] }));
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    BrowserWindow.getAllWindows()[0].reload();
  });
  const inputs = page.getByLabel('Received inputs', { exact: true });
  const old = inputs.getByRole('button', { name: 'Old input beyond the former 200 item limit · to A', exact: true });
  const close = inputs.getByRole('button', { name: 'Closely spaced input with its full text · to B', exact: true });
  await expect(old).toBeVisible(); await expect(close).toBeVisible();
  await close.focus(); await close.press('Enter');
  await expect(page.locator('#track-b')).toHaveAttribute('aria-selected', 'true');
  await page.screenshot({ path: 'research/verify/timeline-marks-20261104/close-inputs.png' });
  await app.evaluate(() => (globalThis as typeof globalThis & { advanceMarks: () => void }).advanceMarks());
  await expect(inputs.getByRole('button', { name: 'Updated old input · to A', exact: true })).toBeVisible();
  await expect(inputs.getByRole('button', { name: 'Removed on the next revision · to A', exact: true })).toHaveCount(0);
  await expect(close).toBeVisible();
});

test('project secret settings remove values and account for retained processes', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const project = { projectId: 'a'.repeat(64), revision: 1, names: ['DEMO_TOKEN'], allNames: ['DEMO_TOKEN'], ready: true };
    const identity = (pid: number) => ({ pid, started_seconds: 1, started_micros: 2 });
    let processes = [
      { identity: identity(100), host: true, kept: false, canStop: false },
      { identity: identity(101), host: false, kept: false, canStop: true },
      { identity: identity(102), host: false, kept: false, canStop: true },
    ];
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, sequence: project.revision, projectSecrets: project, tracks: [{ id: 'track-a', label: 'A', title: 'Work', status: 'ready' }], utterances: [], workers: [] }));
    ipcMain.removeHandler('intercat:project-secret-usage');
    ipcMain.handle('intercat:project-secret-usage', () => [{ nonce: 'host', trackId: 'track-a', repairRunId: 'repair-fixture', revision: 1, names: ['DEMO_TOKEN'], processes }]);
    ipcMain.removeHandler('intercat:project-secret-settings');
    ipcMain.handle('intercat:project-secret-settings', () => ({ project, savedRevision: project.revision, savedNames: project.names, issue: null }));
    ipcMain.removeHandler('intercat:project-secret-remove');
    ipcMain.handle('intercat:project-secret-remove', (_event, request) => {
      if (request.name !== 'DEMO_TOKEN' || request.revision !== 1) throw new Error('Stale removal');
      project.names = []; project.revision = 2;
    });
    ipcMain.removeHandler('intercat:project-secret-apply');
    ipcMain.handle('intercat:project-secret-apply', (_event, revision) => {
      if (revision !== 2) throw new Error('Stale application');
      processes = processes.filter(process => !process.host);
    });
    ipcMain.removeHandler('intercat:project-secret-process');
    ipcMain.handle('intercat:project-secret-process', (_event, request) => {
      if (request.nonce !== 'host') throw new Error('Wrong host');
      if (request.keep) processes.find(process => process.identity.pid === request.identity.pid)!.kept = true;
      else processes = processes.filter(process => process.identity.pid !== request.identity.pid);
    });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
    BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  const settings = page.getByRole('region', { name: 'Project secrets', exact: true });
  await expect(settings).toContainText('Saved to Keychain');
  await expect(settings).toContainText('Repair for Worker A’s process 101');
  await settings.getByRole('button', { name: 'Remove DEMO_TOKEN', exact: true }).click();
  await expect(settings).toContainText('Removed · still in use by 1 host · in use by 2 processes');
  await settings.getByRole('button', { name: 'Apply now', exact: true }).click();
  await expect(settings.getByRole('button', { name: 'Apply now', exact: true })).toHaveCount(0);
  await settings.getByRole('button', { name: 'Keep', exact: true }).first().click();
  await expect(settings.getByRole('button', { name: 'Keep', exact: true })).toHaveCount(1);
  await settings.getByRole('button', { name: 'Stop it', exact: true }).click();
  await expect(settings).toContainText('Removed · in use by 1 process');
  await expect(settings.getByRole('button', { name: 'Stop it', exact: true })).toHaveCount(0);
  await page.screenshot({ path: 'research/verify/project-secrets-20261104/settings.png' });
});

test('unapplied saved secrets remain editable and a failed retry never removes a value', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const project = { projectId: 'a'.repeat(64), revision: 2, names: ['OLD'], allNames: ['OLD'], ready: false };
    let savedRevision = 3; let savedNames = ['EXTRA', 'OLD']; let issue: string | null = 'Saved project secrets could not be applied. Review them in Settings.';
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, sequence: project.revision, projectSecrets: project, projectSecretIssue: issue, tracks: [], utterances: [], workers: [] }));
    ipcMain.removeHandler('intercat:project-secret-settings');
    ipcMain.handle('intercat:project-secret-settings', () => ({ project, savedRevision, savedNames, issue }));
    ipcMain.removeHandler('intercat:project-secret-usage'); ipcMain.handle('intercat:project-secret-usage', () => []);
    ipcMain.removeHandler('intercat:project-secret-retry'); ipcMain.handle('intercat:project-secret-retry', () => { throw new Error('Still over capacity.'); });
    ipcMain.removeHandler('intercat:project-secret-remove');
    ipcMain.handle('intercat:project-secret-remove', (_event, request) => {
      if (request.name !== 'EXTRA' || request.revision !== 3) throw new Error('Stale saved revision.');
      savedRevision = 4; savedNames = ['OLD']; project.revision = 4; project.ready = true; issue = null;
    });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760); BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Review saved secrets', exact: true }).click();
  const settings = page.getByRole('region', { name: 'Project secrets', exact: true });
  await expect(settings).toContainText('Saved to Keychain · not applied');
  await settings.getByRole('button', { name: 'Apply saved values', exact: true }).click();
  await expect(settings.getByRole('button', { name: 'Remove EXTRA', exact: true })).toBeEnabled();
  await page.screenshot({ path: 'research/verify/secret-recovery-20261104/unapplied.png' });
  await settings.getByRole('button', { name: 'Remove EXTRA', exact: true }).click();
  await expect(settings.getByRole('button', { name: 'Remove EXTRA', exact: true })).toHaveCount(0);
  await expect(settings.getByRole('button', { name: 'Remove OLD', exact: true })).toBeEnabled();
  await expect(settings.getByRole('button', { name: 'Apply saved values', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Review saved secrets', exact: true })).toHaveCount(0);
});

test('live worker preview updates independently of saved state and never crosses sessions', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const shared = globalThis as typeof globalThis & { preview: unknown; blockStatus: boolean; previewStops: number };
    shared.preview = { sessionId: 'live-session', turnId: 'turn', itemId: 'item', text: 'Checking the first file…', truncated: false };
    shared.blockStatus = false; shared.previewStops = 0;
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => shared.blockStatus ? new Promise(() => {}) : ({ connected: true, sequence: 9, tracks: [{ id: 'live-a', label: 'A', title: 'Live work', status: 'working' }], utterances: [], workers: [{ trackId: 'live-a', sessionId: 'live-session', phase: 'working', activeTurn: 'turn', approvals: [], prompts: [], output: null }] }));
    ipcMain.removeHandler('intercat:output-flows');
    ipcMain.handle('intercat:output-flows', () => [{ trackId: 'live-a', repairRunId: null, queuedBytes: 0, oldestQueuedMs: 0, droppedDeltas: 0, lagging: false, interrupted: false, preview: shared.preview }]);
    ipcMain.removeHandler('intercat:worker-stop');
    ipcMain.handle('intercat:worker-stop', () => { shared.previewStops += 1; });
    BrowserWindow.getAllWindows()[0].reload();
  });
  const preview = page.getByRole('log', { name: 'Worker A in progress', exact: true });
  await expect(preview).toContainText('Checking the first file…');
  await app.evaluate(() => {
    const shared = globalThis as typeof globalThis & { preview: unknown; blockStatus: boolean };
    shared.blockStatus = true;
    shared.preview = { sessionId: 'live-session', turnId: 'turn', itemId: 'item', text: 'Next file <secret:TOKEN> <script>globalThis.injected = true</script>', truncated: true };
  });
  await expect(preview).toContainText('Next file <secret:TOKEN>');
  await expect(preview).toContainText('…');
  expect(await page.evaluate(() => (globalThis as { injected?: boolean }).injected)).toBeUndefined();
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1040, 760));
  await page.screenshot({ path: 'research/verify/progress-20261104/live-output.png' });
  await page.getByRole('button', { name: 'Stop Worker A', exact: true }).click();
  await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { previewStops: number }).previewStops)).toBe(1);
  await app.evaluate(() => {
    (globalThis as typeof globalThis & { preview: unknown }).preview = { sessionId: 'old-session', turnId: 'turn', itemId: 'item', text: 'Previous connection', truncated: false };
  });
  await expect(preview).toHaveCount(0);
});

test('summarized history stays attached to its selected track', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 8, tracks: [{ id: 'summary-a', label: 'A', title: 'Recovered work', status: 'ready' }, { id: 'summary-b', label: 'B', title: 'Other work', status: 'ready' }], utterances: [], workers: [], timeline: { clock: { epochMs: Date.now(), monoMs: 1000, bootId: 'test', timelineMs: Date.now() }, summaries: [{ trackId: 'summary-a', sessionId: 'retained', repairRunId: null, boundaryCursor: 'opaque' }] } }));
    BrowserWindow.getAllWindows()[0].reload();
  });
  const note = page.getByRole('note').filter({ hasText: 'Earlier history was summarized by the worker.' });
  await page.locator('#track-summary-a').click();
  await expect(note).toBeVisible();
  await page.locator('#track-summary-b').click();
  await expect(note).toHaveCount(0);
  await page.locator('#track-summary-a').click();
  await expect(note).toBeVisible();
  await page.screenshot({ path: 'research/verify/history-recovery-20261104/history-summary.png' });
});

test('stage renders, selects tracks and cuts only a planned turn', async () => {
  const page = await app.firstWindow();
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await expect(page.locator('.timeline')).toHaveAttribute('data-ready', 'true');
  const grid = page.getByRole('grid', { name: 'Tracks' });
  await grid.focus(); await page.keyboard.press('ArrowDown');
  await expect(page.locator('#track-B')).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('3');
  await expect(page.locator('#track-C')).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Control+Tab');
  await expect(page.getByRole('region', { name: 'Desk' })).toBeFocused();
  const cut = page.getByRole('button', { name: 'Cut changelog from A' });
  await cut.focus(); await cut.press('Backspace');
  await expect(cut).toHaveCount(0);
  await expect(page.getByRole('status')).toContainText('current turn is unchanged');
  await expect(page.locator('#track-A')).toContainText('login retry');
  await page.screenshot({ path: 'research/verify/renderer-20261104/stage.png' });
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1040, 760));
  await expect(page.locator('.timeline')).toHaveAttribute('data-future-count', '1');
  const footer = await page.locator('.status-bar').boundingBox();
  expect(footer!.y + footer!.height).toBeLessThanOrEqual((await page.evaluate(() => innerHeight)) + 1);
  await page.screenshot({ path: 'research/verify/renderer-20261104/stage-small.png' });
  expect(errors).toEqual([]);
});

test('boundary stop holds only the selected worker without sending an immediate Stop', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const state = { connected: true, sequence: 1, workerConfigured: true,
      tracks: ['A', 'B'].map(id => ({ id, label: id, title: id, status: 'working' })),
      workers: ['A', 'B'].map(id => ({ trackId: id, phase: 'working', sessionId: id, activeTurn: id, approvals: [], prompts: [], checkpoint: null })),
      utterances: [] };
    const calls: { kind: string; request: { trackId: string } }[] = [];
    Object.assign(globalThis, { boundaryStopCalls: calls });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    for (const kind of ['hold', 'stop']) {
      ipcMain.removeHandler('intercat:worker-' + kind);
      ipcMain.handle('intercat:worker-' + kind, (_event, request) => calls.push({ kind, request }));
    }
  });
  await expect(page.getByRole('row')).toHaveCount(2);
  await page.getByRole('grid', { name: 'Tracks' }).press('1');
  await page.evaluate(() => document.activeElement?.dispatchEvent(new KeyboardEvent('keydown', { key: '>', code: 'Period', shiftKey: true, metaKey: true, bubbles: true })));
  await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { boundaryStopCalls: unknown[] }).boundaryStopCalls.length)).toBe(1);
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { boundaryStopCalls: { kind: string; request: { trackId: string } }[] }).boundaryStopCalls)).toMatchObject([{ kind: 'hold', request: { trackId: 'A' } }]);
});

test('starting a held input retries its operation and leaves other held inputs alone', async () => {
  const page=await app.firstWindow();
  await app.evaluate(({ipcMain}) => {
    const words=['First held request','Second held request'].map((text,index)=>({
      id:'held-'+index,text,state:'held',revision:2,createdAt:Date.now(),attachments:[],target:{mode:'lock',trackId:'A'},reason:null,
      decision:{kind:'extend',trackId:'A',boundary:null,command:null,confidence:1,inferenceMs:1,timedOut:false},
    }));
    const state={connected:true,sequence:1,workerConfigured:true,tracks:[{id:'A',label:'A',title:'Held worker',status:'waiting'}],workers:[],utterances:words};
    const calls:unknown[]=[];
    Object.assign(globalThis,{heldStartCalls:calls});
    ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
    ipcMain.removeHandler('intercat:start-held');ipcMain.handle('intercat:start-held',(_event,request)=>{
      calls.push(request);
      if(calls.length===1) throw new Error('Acknowledgement unavailable');
      const word=state.utterances.find(word=>word.id===request.utteranceId)!;
      word.state='placed';word.revision++;state.sequence++;
      return word;
    });
  });
  const first=page.getByRole('button',{name:'Start request: First held request',exact:true});
  await first.click();
  await expect(page.getByRole('status')).toContainText('This request could not start');
  await first.click();
  await expect(first).toHaveCount(0);
  await expect(page.getByRole('button',{name:'Start request: Second held request',exact:true})).toBeVisible();
  const calls=await app.evaluate(()=>(globalThis as typeof globalThis & {heldStartCalls:unknown[]}).heldStartCalls);
  expect(calls).toHaveLength(2);
  expect(calls[0]).toEqual(calls[1]);
});

test('Stop all turns shows remaining processes and Resume keeps existing requests held', async () => {
  const page=await app.firstWindow();
  await app.evaluate(({ipcMain,BrowserWindow})=>{
    const state={connected:true,sequence:1,workerConfigured:true,dispatchPaused:false,runningProcesses:3,
      tracks:[{id:'A',label:'A',title:'First worker',status:'working'},{id:'B',label:'B',title:'Second worker',status:'working'}],
      workers:['A','B'].map(id=>({trackId:id,phase:'working',activeTurn:id as string|null,approvals:[],prompts:[],checkpoint:null})),
      utterances:[{id:'word',text:'Existing queued request',state:'placed',revision:1,createdAt:Date.now(),attachments:[],target:{mode:'lock',trackId:'A'},decision:{kind:'extend',trackId:'A',boundary:'after-turn',inferenceMs:1,confidence:1},reason:null}]};
    const calls:boolean[]=[];Object.assign(globalThis,{globalPauseCalls:calls});
    ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
    ipcMain.removeHandler('intercat:dispatch-pause');ipcMain.handle('intercat:dispatch-pause',(_event,request)=>{
      calls.push(request.paused);state.dispatchPaused=request.paused;state.sequence++;
      if(request.paused){state.workers.forEach(worker=>{worker.activeTurn=null;worker.phase='held';});state.tracks.forEach(track=>{track.status='stopped';});state.utterances[0].state='held';}
      return {paused:request.paused};
    });
    BrowserWindow.getAllWindows()[0].setSize(1040,760);
  });
  await page.getByRole('button',{name:'Stop all turns, 2 running',exact:true}).click();
  await expect(page.getByRole('status').filter({hasText:'Turns stopped'})).toContainText('3 processes still running');
  await expect(page.getByRole('button',{name:'Resume',exact:true})).toBeEnabled();
  await expect(page.locator('.track-status.stopped')).toHaveCount(2);
  await expect(page.locator('.track-status.stopped').first()).toHaveText('■ stopped');
  await page.getByRole('button',{name:'Resume',exact:true}).click();
  await expect(page.getByRole('button',{name:'Resume',exact:true})).toHaveCount(0);
  await expect(page.getByRole('button',{name:'Start request: Existing queued request',exact:true})).toBeVisible();
  expect(await app.evaluate(()=>(globalThis as typeof globalThis & {globalPauseCalls:boolean[]}).globalPauseCalls)).toEqual([true,false]);
});

test('live steps open their recorded output and patch without showing sample code', async () => {
  const page = await app.firstWindow();
  const now = Date.now();
  const clip = { id: 'f121150b-0c3a-5e2d-9aa4-f71cc5b80e1c', trackId: 'actual-track', ordinal: 14, sessionId: 'actual-session', turnId: 'actual-turn', category: 'step', label: 'edit_file actual.cjs', kind: 'edit', start: now - 130000, end: now - 45000, running: false, generationEnd: now - 90000, approximate: false, toolTicks: [{ itemId: 'actual-item', tool: 'edit_file', status: 'completed' }] };
  await app.evaluate(({ ipcMain, BrowserWindow }, value) => {
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 7, workerConfigured: true, tracks: [{ id: value.clip.trackId, label: 'A', title: 'Actual work', status: 'done' }], utterances: [], workers: [], timeline: { summaries: [], clock: { epochMs: value.now, monoMs: 1000, bootId: 'boot', timelineMs: value.now } } }));
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [value.clip], removed: [], marks: [{ id: "actual-session/mark", trackId: value.clip.trackId, at: value.now - 130000, inputId: "request", text: "Fix actual.cjs", repairRunId: null }], removedMarks: [] }));
    ipcMain.removeHandler('intercat:clip-detail');
    ipcMain.handle('intercat:clip-detail', (_event, id) => {
      if (id !== value.clip.id) throw new Error('Wrong step requested');
      return { clip: value.clip, items: [{ itemId: 'actual-item', kind: 'toolCall', revision: 3, status: 'completed', tool: 'edit_file', text: '<script>globalThis.__injected = true</script>', patchRef: { id: 'patch' }, outputs: { patchRef: { encoding: 'utf8', eof: true, content: JSON.stringify({ files: [{ path: 'actual.cjs', hunks: [{ oldStart: 1, oldLines: 1, newStart: 1, newLines: 1, lines: ['-actual = false;', '+actual = true;'] }] }] }) } } }] };
    });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, { clip, now });
  const button = page.getByRole('button', { name: 'Open step A:14: edit_file actual.cjs', exact: true });
  await expect(button).toBeAttached(); await button.focus(); await button.press('Enter');
  const details = page.getByLabel('Step details');
  await expect(details).toContainText('+actual = true;');
  await expect(page.locator('.step-artifact pre')).toBeInViewport();
  await expect(details).toContainText('revision 3');
  await expect(details).not.toContainText('services/logger');
  expect(await page.evaluate(() => (globalThis as { __injected?: boolean }).__injected)).toBeUndefined();
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toBeVisible();
  await page.screenshot({ path: 'research/verify/renderer-20261104/real-step-fixture.png' });
  await page.getByRole('button', { name: 'Close step preview', exact: true }).click();
  await app.evaluate(({ ipcMain }, clip) => {
    const global = globalThis as typeof globalThis & { restores: unknown[] }; global.restores = [];
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [clip], removed: [], marks: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:clip-detail');
    ipcMain.handle('intercat:clip-detail', () => ({ clip, items: [], deletionSnapshots: [{ id: '01a1a2b6-4589-7688-87f6-5febb3a36fac', restoreOperationId: '01a1a2b6-5000-7000-8000-000000000001', trackId: clip.trackId, itemId: 'actual-item', sha: 'a'.repeat(40), reference: 'refs/intercat/test', paths: ['actual.cjs'], absent: [], directories: [], uncovered: [], complete: true, purpose: 'before-delete' }] }));
    ipcMain.removeHandler('intercat:restore-deletion');
    ipcMain.handle('intercat:restore-deletion', (_event, request) => { global.restores.push(request); return { snapshotId: request.snapshotId, backupId: 'backup', files: 1 }; });
  }, clip);
  await button.focus(); await button.press('Enter');
  const recovery = page.getByRole('region', { name: 'Before deletion', exact: true });
  await expect(recovery).toContainText('An unfinished restore is waiting');
  await recovery.getByRole('button', { name: 'Restore saved files', exact: true }).click();
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { restores: unknown[] }).restores.length)).toBe(0);
  await recovery.getByRole('button', { name: 'Restore now', exact: true }).click();
  await expect(recovery).toContainText('Restored 1 saved files.');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { restores: { opId: string }[] }).restores[0].opId)).toBe('01a1a2b6-5000-7000-8000-000000000001');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { restores: { repairRunId: string | null }[] }).restores[0].repairRunId)).toBeNull();
  await page.getByRole('button', { name: 'Close step preview', exact: true }).click();
  await app.evaluate(({ ipcMain }, clip) => {
    const global = globalThis as typeof globalThis & { restores: unknown[] }; global.restores = [];
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [clip], removed: [], marks: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:clip-detail');
    ipcMain.handle('intercat:clip-detail', () => ({ clip, items: [], deletionSnapshots: [{ id: '01a1a2b6-4589-7688-87f6-5febb3a36fac', restoreOperationId: '01a1a2b6-5000-7000-8000-000000000002', trackId: clip.trackId, repairRunId: '01a1a2b6-5000-7000-8000-000000000003', itemId: 'actual-item', sha: 'a'.repeat(40), reference: 'refs/intercat/repair', paths: ['actual.cjs'], absent: [], directories: [], uncovered: [], complete: true, purpose: 'before-delete' }] }));
    ipcMain.removeHandler('intercat:restore-deletion');
    ipcMain.handle('intercat:restore-deletion', (_event, request) => { global.restores.push(request); if (global.restores.length === 1) throw new Error('Acknowledgement lost'); return { snapshotId: request.snapshotId, backupId: 'backup', files: 1 }; });
  }, clip);
  await button.focus(); await button.press('Enter');
  await recovery.getByRole('button', { name: 'Restore saved files', exact: true }).click();
  await expect(recovery).toContainText('Stop the repair');
  await recovery.getByRole('button', { name: 'Restore now', exact: true }).click();
  await expect(recovery).toContainText('You can retry this restore.');
  await recovery.getByRole('button', { name: 'Restore now', exact: true }).click();
  await expect(recovery).toContainText('The repair is held.');
  const restored = await app.evaluate(() => (globalThis as typeof globalThis & { restores: { opId: string; repairRunId: string }[] }).restores);
  expect(restored[0]).toEqual(restored[1]);
  expect(restored[0].repairRunId).toBe('01a1a2b6-5000-7000-8000-000000000003');
});

test('packaged Monaco and PDF workers run under CSP without eval or remote connections', async () => {
  const page = await app.firstWindow();
  const requests: string[] = [];
  page.on('request', request => requests.push(request.url()));
  const response = await page.goto('intercat://app/?view=diagnostics');
  expect(response?.headers()['content-security-policy']).toContain("connect-src 'none'");
  expect(response?.headers()['content-security-policy']).not.toContain('unsafe-eval');
  await expect(page.getByTestId('diagnostics')).toContainText('"state": "passed"', { timeout: 20000 });
  const result = JSON.parse((await page.getByTestId('diagnostics').textContent())!);
  expect(result.monaco.workers).toBeGreaterThan(0);
  expect(result.monaco.diffChanges).toBe(1);
  expect(result.pdf).toEqual({ pages: 1, text: 'Intercat local PDF', realWorker: true });
  expect(result.violations).toEqual([]);
  expect(requests.filter(url => /^https?:/.test(url))).toEqual([]);
  expect(await page.evaluate(() => ({ require: typeof (globalThis as { require?: unknown }).require, process: typeof (globalThis as { process?: unknown }).process }))).toEqual({ require: 'undefined', process: 'undefined' });
  expect(await app.evaluate(({ app }) => app.getAppMetrics().filter(process => process.type === 'Tab').map(process => process.sandboxed))).toEqual([true]);
  await writeFile('research/verify/renderer-20261104/result.json', JSON.stringify({ ...result, csp: response?.headers()['content-security-policy'], remoteRequests: requests.filter(url => /^https?:/.test(url)), appVersion: await app.evaluate(({ app }) => app.getVersion()) }, null, 2));
  await page.screenshot({ path: 'research/verify/renderer-20261104/diagnostics.png' });
  // DevTools evaluation itself can bypass CSP's eval restriction. The actual
  // checks must run from the packaged renderer's event handler.
  await page.getByRole('button', { name: 'Test blocked operations' }).click();
  await expect(page.getByTestId('blocked-operations')).toHaveText('{"evalBlocked":true,"fetchBlocked":true}');
});

test('approval and long worker reports leave the composer usable in a small window', async () => {
  const page = await app.firstWindow();
  const fixture = {
    connected: true, workerConfigured: true,
    project: { name: 'Fixture', path: '/fixture', root: '/fixture', commonDir: '/fixture/.git', base: 'a'.repeat(40), baseBranch: 'refs/heads/main' },
    tracks: ['A', 'B', 'C'].map(id => ({ id, label: id, title: `Dummy work ${id}`, status: 'waiting' })),
    workers: ['A', 'B', 'C'].map(id => ({ trackId: id, phase: id === 'C' ? 'approval' : 'ready', sessionId: id, identity: null, detail: null, activeTurn: null, checkpointPending: false, checkpoint: null, prompts: [], output: 'Completed dummy work.\n'.repeat(20), approvals: id === 'C' ? [{ id: 'approval', requirement: { approvalId: 'approval', sourceIndex: 0 }, subject: 'git push origin HEAD:probe\n'.repeat(12), reason: { kind: 'push', message: 'Review this push' }, canAllow: true, canDeny: true }] : [] })),
    utterances: [], editor: { phase: 'ready' },
  };
  await app.evaluate(({ BrowserWindow, ipcMain }, state) => { ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state); BrowserWindow.getAllWindows()[0].setSize(1040, 760); }, fixture);
  const card = page.getByRole('group', { name: 'Approval for Worker C' });
  await expect(card).toBeVisible();
  const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
  expect(viewport.width).toBe(1040);
  const composer = await page.getByRole('textbox', { name: 'Request draft' }).boundingBox();
  const footer = await page.locator('.status-bar').boundingBox();
  expect(composer!.y + composer!.height).toBeLessThanOrEqual(viewport.height);
  expect(footer!.y + footer!.height).toBeLessThanOrEqual(viewport.height);
  await expect(card.getByRole('button', { name: 'Allow once' })).toBeDisabled();
  await card.locator('pre').evaluate(element => { element.scrollTop = element.scrollHeight; element.dispatchEvent(new Event('scroll')); });
  await expect(card.getByRole('button', { name: 'Allow once' })).toBeEnabled();
  await card.getByRole('button', { name: 'Don’t run it' }).scrollIntoViewIfNeeded();
  const button = await card.getByRole('button', { name: 'Don’t run it' }).boundingBox();
  expect(button!.y + button!.height).toBeLessThan(composer!.y);
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Still able to write while C waits.');
  await expect(page.getByRole('button', { name: 'Put', exact: true })).toBeEnabled();
  await page.screenshot({ path: 'research/verify/execution-ui-20261104/layout-small.png' });
});

test('scheduled work has no allow button', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, workerConfigured: true, tracks: [{ id: 'A', label: 'A', title: 'Waiting job', status: 'waiting' }], workers: [{ trackId: 'A', phase: 'approval', prompts: [], approvals: [{ id: 'scheduled', requirement: { sourceIndex: 0 }, subject: 'crontab jobs.txt', reason: { kind: 'schedule', message: 'Scheduled jobs aren’t supported. Keep this work on the timeline.' }, canAllow: false, canDeny: true }] }], utterances: [] }));
  });
  const card = page.getByRole('group', { name: 'Approval for Worker A' });
  await expect(card).toContainText('Scheduled jobs aren’t supported');
  await expect(card.getByRole('button', { name: 'Allow once' })).toHaveCount(0);
  await expect(card.getByRole('button', { name: 'Don’t run it' })).toBeEnabled();
});

test('a recovered approval awaiting confirmation cannot be answered again and still allows Stop', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }, token) => {
    const state = { connected: true, sequence: 1, tracks: [{ id: 'recovered', label: 'A', title: 'Recovered work', status: 'waiting' }], utterances: [], workers: [{ trackId: 'recovered', phase: 'approval', sessionId: 'session', identity: null, detail: null, activeTurn: token, checkpointPending: false, checkpoint: null, prompts: [], output: null, approvals: [{ id: 'known', requirement: { sourceIndex: 0 }, subject: 'echo test', reason: { kind: 'reconnecting', message: 'The previous answer is still awaiting host confirmation.' }, canAllow: false, canDeny: false }] }] };
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:worker-stop'); ipcMain.handle('intercat:worker-stop', (_event, request) => { if (request.trackId !== 'recovered') throw new Error('Wrong worker'); state.workers[0].phase = 'held'; state.sequence++; });
  }, '01900000-0000-7000-8000-000000000001');
  const card = page.getByRole('group', { name: 'Approval for Worker A' });
  await expect(card).toContainText('awaiting host confirmation');
  await expect(card.getByRole('button', { name: 'Allow once' })).toHaveCount(0);
  await expect(card.getByRole('button', { name: 'Don’t run it' })).toBeDisabled();
  await page.getByRole('button', { name: 'Stop Worker A', exact: true }).click();
  await expect(page.locator('.worker-controls')).toContainText('held');
});

test('external effects retain their separate outcomes in a step', async () => {
  const page = await app.firstWindow();
  const now = Date.now();
  const clip = { id: 'e121150b-0c3a-5e2d-9aa4-f71cc5b80e1c', trackId: 'A', ordinal: 2, sessionId: 'session', turnId: 'turn', category: 'step', label: 'bash work', kind: 'shell', start: now - 90000, end: now - 30000, running: false, generationEnd: now - 70000, approximate: false, effectCount: 3, toolTicks: [] };
  await app.evaluate(({ ipcMain }, value) => {
    const effects = [{ id: 'one', kind: 'push', target: 'origin', outcome: 'done' }, { id: 'two', kind: 'send', target: 'outside.invalid:443', outcome: 'failed' }, { id: 'three', kind: 'deploy', target: 'staging', outcome: 'unknown' }].map(effect => ({ ...effect, trackId: 'A', sessionId: 'session', itemId: 'item', turnId: 'turn', clipId: value.clip.id, ruleId: 'fixture', at: value.now, running: false, reason: null }));
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, tracks: [{ id: 'A', label: 'A', title: 'Fixture', status: 'done' }], workers: [], effects, timeline: { summaries: [], clock: { timelineMs: value.now } } }));
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [value.clip], removed: [], marks: [{ id: "actual-session/mark", trackId: value.clip.trackId, at: value.now - 130000, inputId: "request", text: "Fix actual.cjs", repairRunId: null }], removedMarks: [] }));
    ipcMain.removeHandler('intercat:clip-detail');
    ipcMain.handle('intercat:clip-detail', () => ({ clip: value.clip, items: [], effects }));
  }, { clip, now });
  const button = page.getByRole('button', { name: 'Open step A:2: bash work', exact: true });
  await expect(button).toHaveAttribute('title', /3 effects outside Git/);
  await button.focus(); await button.press('Enter');
  const effects = page.getByLabel('Step details').getByRole('region', { name: 'Effects outside Git' });
  await expect(effects).toContainText('completed');
  await expect(effects).toContainText('failed');
  await expect(effects).toContainText('outcome unknown');
  await expect(effects).toContainText('Restoring files does not undo these actions.');
});

function questionFixture() {
  return { connected: true, workerConfigured: true, tracks: [{ id: 'A', label: 'A', title: '質問のテスト', status: 'working' }], utterances: [], editor: { phase: 'ready' }, workers: [{ trackId: 'A', phase: 'question', sessionId: 'session', identity: null, detail: null, activeTurn: 'turn', checkpointPending: false, checkpoint: null, approvals: [], output: null, prompts: [{ revision: 'a'.repeat(64), phase: 'open', operation: null, request: { sessionId: 'session', userInputId: 'prompt', turnId: 'turn', questions: [
    { id: 'one', header: '一つ選ぶ', question: '採用する方式を選んでください。'.repeat(8), selection: { mode: 'single' }, options: [{ label: 'First', description: '第一案' }, { label: 'Second', description: '第二案' }] },
    { id: 'many', header: '二つ選ぶ', question: '必要な項目を二つ選んでください。'.repeat(8), selection: { mode: 'multiple', minSelections: 2, maxSelections: 2 }, options: [{ label: 'Red' }, { label: 'Green' }, { label: 'Blue' }] },
    { id: 'free', header: '補足', question: '補足したい内容を説明してください。'.repeat(8), selection: { mode: 'single' }, options: [{ label: 'Yes' }, { label: 'No' }] },
  ] } }] }] };
}

test('a redirect shows its stop boundary and a held correction needs an explicit Start', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const word = { attachments: [], id: 'redirect-input', text: '別の方法で修正して', createdAt: Date.now(), state: 'redirecting', revision: 1, target: { mode: 'none' }, decision: { kind: 'trim', trackId: 'A', boundary: 'now', command: null, confidence: 1, inferenceMs: 1, timedOut: false }, reason: null };
    const state = { connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: '作業中', status: 'working' }], utterances: [word], redirects: [{ inputId: word.id, revision: 1, trackId: 'A', sessionId: 'session', turnId: 'turn', phase: 'stopping' }] };
    const shared = globalThis as typeof globalThis & { redirectFixture: typeof state; redirectCalls: unknown[] };
    shared.redirectFixture = state; shared.redirectCalls = [];
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, request) => { shared.redirectCalls.push(request); return { ...word, state: 'placed' }; });
  });
  const chip = page.locator('.request-chip').filter({ hasText: '別の方法で修正して' });
  await expect(chip).toContainText('redirect A · stopping');
  await expect(chip.getByRole('button', { name: /Cut request/ })).toHaveCount(0);
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { redirectFixture: { sequence: number; utterances: { state: string }[]; redirects: { phase: string }[] } }).redirectFixture;
    state.sequence++; state.utterances[0].state = 'held'; state.redirects[0].phase = 'held';
  });
  await expect(chip).toContainText('redirect A · held');
  await chip.getByRole('button', { name: 'Start request: 別の方法で修正して' }).click();
  await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { redirectCalls: unknown[] }).redirectCalls.length)).toBe(1);
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { redirectCalls: { resolution: unknown }[] }).redirectCalls[0].resolution)).toEqual({ action: 'retry', target: { mode: 'lock', trackId: 'A' } });
});
async function showQuestionFixture(app: ElectronApplication) {
  const page = await app.firstWindow();
  await app.evaluate(({ BrowserWindow, ipcMain }, fixture) => {
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => fixture);
    ipcMain.removeHandler('intercat:worker-respond');
    const state = globalThis as typeof globalThis & { questionResponses: unknown[] };
    state.questionResponses = [];
    ipcMain.handle('intercat:worker-respond', (_event, response) => { state.questionResponses.push(response); return null; });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, questionFixture());
  await expect(page.getByRole('group', { name: 'Questions for Worker A' })).toBeVisible();
  return page;
}

test('three questions validate together and explanation remains separate from choices', async () => {
  const page = await showQuestionFixture(app);
  const card = page.getByRole('group', { name: 'Questions for Worker A' });
  const send = card.getByRole('button', { name: 'Send answers', exact: true });
  await expect(send).toBeDisabled();
  await card.getByRole('radio', { name: 'First', exact: true }).check();
  await card.getByRole('checkbox', { name: 'Red', exact: true }).check();
  await expect(send).toBeDisabled();
  await card.getByRole('checkbox', { name: 'Blue', exact: true }).check();
  await card.getByRole('group', { name: '補足', exact: true }).getByRole('checkbox', { name: 'Write an answer' }).check();
  await card.getByRole('textbox', { name: 'Answer to 補足' }).fill('補足の回答');
  await card.getByRole('textbox', { name: 'Note for 補足' }).fill('注記');
  await expect(send).toBeEnabled();
  const composer = await page.getByRole('textbox', { name: 'Request draft' }).boundingBox();
  const buttons = await card.locator('.question-actions').boundingBox();
  expect(buttons!.y + buttons!.height).toBeLessThanOrEqual(composer!.y);
  await send.click();
  const first = await app.evaluate(() => (globalThis as typeof globalThis & { questionResponses: { response: unknown }[] }).questionResponses[0]);
  expect(first.response).toEqual({ action: 'answer', answers: [{ questionId: 'one', selectedLabel: 'First' }, { questionId: 'many', selectedLabels: ['Red', 'Blue'] }, { questionId: 'free', freeText: '補足の回答', note: '注記' }] });
  await card.getByRole('button', { name: 'Explain instead', exact: true }).click();
  await card.getByRole('textbox', { name: 'Explanation', exact: true }).fill('選択肢以外の説明');
  await card.getByRole('button', { name: 'Send explanation', exact: true }).click();
  const second = await app.evaluate(() => (globalThis as typeof globalThis & { questionResponses: { response: unknown }[] }).questionResponses[1]);
  expect(second.response).toEqual({ action: 'explain', content: '選択肢以外の説明' });
  await page.screenshot({ path: 'research/verify/question-ui-20261104/explanation.png' });
});

test('question Stop sends only the completed partial answers', async () => {
  const page = await showQuestionFixture(app);
  const card = page.getByRole('group', { name: 'Questions for Worker A' });
  await card.getByRole('radio', { name: 'Second', exact: true }).check();
  await card.getByRole('checkbox', { name: 'Red', exact: true }).check();
  await card.getByRole('button', { name: 'Stop', exact: true }).click();
  const result = await app.evaluate(() => (globalThis as typeof globalThis & { questionResponses: { response: unknown }[] }).questionResponses[0]);
  expect(result.response).toEqual({ action: 'stop', answers: [{ questionId: 'one', selectedLabel: 'Second' }] });
});

test('report replies keep their question when browsing another track', async () => {
  const page = await app.firstWindow();
  const fixture = { ...questionFixture(), tracks: ['A', 'B'].map(id => ({ id, label: id, title: `Work ${id}`, status: 'waiting' })), workers: [], reportQuestions: [{ id: 'question-A', reportId: 'report', trackId: 'A', turnId: 'turn', revision: 2, kind: 'question', text: 'What should the output contain?', state: 'open', replyUtteranceId: null }] };
  await app.evaluate(({ ipcMain }, state) => {
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    const globals = globalThis as typeof globalThis & { reportResponses: unknown[] }; globals.reportResponses = [];
    ipcMain.removeHandler('intercat:report-response'); ipcMain.handle('intercat:report-response', (_event, request) => { globals.reportResponses.push(request); return { id: request.opId, text: request.answer.text, attachments: [], state: 'placed', revision: 1, target: { mode: 'lock', trackId: 'A' }, decision: { kind: 'extend', trackId: 'A' }, createdAt: Date.now(), replyTo: { id: request.questionId, revision: request.revision } }; });
  }, fixture);
  await page.getByRole('group', { name: 'Question from Worker A' }).getByRole('button', { name: 'Reply', exact: true }).click();
  await page.locator('#track-B').click();
  await expect(page.locator('.reply-target')).toContainText('Reply to Worker A');
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Use the approved output.');
  await page.getByRole('button', { name: 'Send reply', exact: true }).click();
  const responses = await app.evaluate(() => (globalThis as typeof globalThis & { reportResponses: { questionId: string; revision: number; answer: unknown }[] }).reportResponses);
  expect(responses).toHaveLength(1);
  expect(responses[0]).toMatchObject({ questionId: 'question-A', revision: 2, answer: { action: 'reply', text: 'Use the approved output.' } });
});

test('file recovery resumes its saved operation after a lost confirmation and reload', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const state = { connected: true, workerConfigured: true, sequence: 1, tracks: [{ id: 'recover', label: 'A', title: 'Interrupted work', status: 'waiting' }], utterances: [], pickups: [] as { opId: string; trackId: string; sessionId: string; phase: string; snapshotSha: string }[], workers: [{ trackId: 'recover', phase: 'lost', sessionId: 'old-session', identity: null, detail: 'The old session cannot be resumed.', activeTurn: null, checkpointPending: false, checkpoint: null, prompts: [], output: null, approvals: [] }] };
    const calls: unknown[] = []; (globalThis as typeof globalThis & { pickupCalls: unknown[] }).pickupCalls = calls;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:pickup'); ipcMain.handle('intercat:pickup', (_event, request) => {
      calls.push(request);
      if (calls.length === 1) {
        state.pickups = [{ ...request, phase: 'awaiting-receipt', snapshotSha: 'saved' }]; state.workers[0].phase = 'after-connection-loss'; state.sequence++;
        throw new Error('Fingerprint confirmation was interrupted.');
      }
      state.workers[0].phase = 'retry'; state.sequence++; return { ...state.pickups[0], phase: 'ready' };
    });
    BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Pick up from files', exact: true }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'Fingerprint confirmation was interrupted.' })).toBeVisible();
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].reload());
  await page.getByRole('button', { name: 'Retry file recovery', exact: true }).click();
  await expect(page.locator('.worker-controls')).toContainText('retry');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { pickupCalls: unknown[] }).pickupCalls);
  expect(calls).toHaveLength(2); expect(calls[1]).toEqual(calls[0]);
});

test('a worker held after restart can stop its recorded processes without resuming history', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const state = { connected: true, workerConfigured: true, sequence: 1, tracks: [{ id: 'recovery-worker', label: 'A', title: 'Interrupted work', status: 'waiting' }], utterances: [], workers: [{ trackId: 'recovery-worker', phase: 'needs-recovery', sessionId: 'retained', identity: null, detail: 'The previous worker did not close cleanly.', activeTurn: 'turn', checkpointPending: true, checkpoint: null, prompts: [], output: null, approvals: [] }] };
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:worker-stop'); ipcMain.handle('intercat:worker-stop', (_event, request) => {
      if (request.trackId !== 'recovery-worker') throw new Error('Wrong worker');
      state.workers[0].phase = 'after-connection-loss'; state.workers[0].detail = 'stopped · connection lost'; state.sequence++;
    });
    BrowserWindow.getAllWindows()[0].reload();
  });
  await expect(page.getByRole('button', { name: 'Retry worker', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Start Worker A', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Stop here', exact: true }).click();
  await expect(page.locator('.worker-controls')).toContainText('stopped · connection lost');
  await expect(page.getByRole('button', { name: 'Stop here', exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Start Worker A', exact: true })).toBeDisabled();
});

test('lost workers offer retry and stop while stale approvals stay disabled', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const state = { connected: true, workerConfigured: true, sequence: 1, tracks: [{ id: 'lost-worker', label: 'A', title: 'Interrupted work', status: 'waiting' }], utterances: [], workers: [{ trackId: 'lost-worker', phase: 'lost', sessionId: 'retained', identity: null, detail: 'The worker lost its session. Its processes may still be running.', activeTurn: 'turn', checkpointPending: true, checkpoint: null, prompts: [], output: null, approvals: [{ id: 'old', requirement: { sourceIndex: 0 }, subject: 'echo old', reason: { kind: 'review', message: 'Previous request' }, canAllow: true, canDeny: true }] }] };
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:worker-retry'); ipcMain.handle('intercat:worker-retry', (_event, request) => { if (request.trackId !== 'lost-worker') throw new Error('Wrong worker'); state.workers[0].phase = 'reconnecting'; state.workers[0].detail = 'Reconnecting · attempt 1 of 3'; state.sequence++; });
    ipcMain.removeHandler('intercat:worker-stop'); ipcMain.handle('intercat:worker-stop', (_event, request) => { if (request.trackId !== 'lost-worker') throw new Error('Wrong worker'); state.workers[0].phase = 'after-connection-loss'; state.workers[0].detail = 'stopped · connection lost'; state.workers[0].approvals = []; state.sequence++; });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  });
  const card = page.getByRole('group', { name: 'Approval for Worker A' });
  await expect(page.getByRole('button', { name: 'Retry connection', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Stop here', exact: true })).toBeVisible();
  await expect(card.getByRole('button', { name: 'Allow once', exact: true })).toBeDisabled();
  await expect(card.getByRole('button', { name: 'Don’t run it', exact: true })).toBeDisabled();
  await page.screenshot({ path: 'research/verify/reconnect-20261104/lost-worker-actions.png' });
  await page.getByRole('button', { name: 'Retry connection', exact: true }).click();
  await expect(page.locator('.worker-controls')).toContainText('reconnecting');
  await expect(page.getByRole('button', { name: 'Start Worker A', exact: true })).toBeDisabled();
  await expect(card.getByRole('button', { name: 'Allow once', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Stop Worker A', exact: true }).click();
  await expect(page.locator('.worker-controls')).toContainText('stopped · connection lost');
  await expect(card).toHaveCount(0);
  await page.screenshot({ path: 'research/verify/reconnect-20261104/lost-worker.png' });
});

test('secret form keeps values out of ordinary answers and retries the same operation', async () => {
  const page = await showQuestionFixture(app);
  await app.evaluate(({ ipcMain }) => {
    const shared = globalThis as typeof globalThis & { secretRequests: Record<string, string>[] };
    shared.secretRequests = [];
    ipcMain.removeHandler('intercat:worker-secret');
    ipcMain.handle('intercat:worker-secret', (_event, request) => {
      shared.secretRequests.push(request);
      if (shared.secretRequests.length === 1) throw new Error('Fixture lost acknowledgement');
      return { accepted: true };
    });
  });
  const card = page.getByRole('group', { name: 'Questions for Worker A' });
  await card.getByRole('button', { name: 'Provide a secret', exact: true }).click();
  await card.getByRole('radio', { name: 'This time only', exact: true }).check();
  const submit = card.getByRole('button', { name: 'Use for this worker', exact: true });
  await card.getByLabel('Secret variable name', { exact: true }).fill('PATH');
  await card.getByLabel('Secret value', { exact: true }).fill('dummy-secret-input-value');
  await expect(submit).toBeDisabled();
  await expect(card.getByLabel('Secret value', { exact: true })).toHaveAttribute('type', 'password');
  await card.getByLabel('Secret variable name', { exact: true }).fill('NPM_TOKEN');
  await expect(submit).toBeEnabled();
  const footer = card.locator('.question-actions');
  const footerBox = await footer.boundingBox();
  const composer = await page.getByRole('textbox', { name: 'Request draft' }).boundingBox();
  expect(footerBox!.y + footerBox!.height).toBeLessThanOrEqual(composer!.y);
  const scope = card.getByText('This time only · this worker, until its host stops. Not kept in Intercat’s log.', { exact: true });
  await expect(scope).toBeInViewport();
  const scopeBox = await scope.boundingBox();
  expect(scopeBox!.y + scopeBox!.height).toBeLessThanOrEqual(footerBox!.y);
  await page.screenshot({ path: 'research/verify/secret-input-20261104/form.png' });
  await submit.click();
  await expect(card.getByRole('alert')).toContainText('could not be confirmed');
  await submit.click();
  await expect(card.getByRole('button', { name: 'Provide a secret', exact: true })).toBeVisible();
  const requests = await app.evaluate(() => (globalThis as typeof globalThis & { secretRequests: Record<string, string>[] }).secretRequests);
  expect(requests).toHaveLength(2);
  expect(requests[1]).toEqual(requests[0]);
  expect(requests[0]).toMatchObject({ name: 'NPM_TOKEN', value: 'dummy-secret-input-value', trackId: 'A', sessionId: 'session', question: { kind: 'msp', id: 'prompt', revision: 'a'.repeat(64) } });
  expect(requests[0].opId).not.toBe(requests[0].fingerprintOpId);
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { questionResponses: unknown[] }).questionResponses)).toEqual([]);
  await card.getByRole('button', { name: 'Provide a secret', exact: true }).click();
  await card.getByRole('radio', { name: 'This time only', exact: true }).check();
  await expect(card.getByLabel('Secret value', { exact: true })).toHaveValue('');
});

test('secret preparation keeps Stop available while its acknowledgement is pending', async () => {
  const page = await showQuestionFixture(app);
  await app.evaluate(({ ipcMain }) => {
    ipcMain.removeHandler('intercat:worker-secret');
    ipcMain.handle('intercat:worker-secret', () => new Promise(() => {}));
  });
  const card = page.getByRole('group', { name: 'Questions for Worker A' });
  await card.getByRole('button', { name: 'Provide a secret', exact: true }).click();
  await card.getByRole('radio', { name: 'This time only', exact: true }).check();
  await card.getByLabel('Secret variable name', { exact: true }).fill('NPM_TOKEN');
  await card.getByLabel('Secret value', { exact: true }).fill('dummy-secret-stop-value');
  await card.getByRole('button', { name: 'Use for this worker', exact: true }).click();
  await expect(card.getByRole('button', { name: 'Preparing…', exact: true })).toBeDisabled();
  await expect(card.getByRole('button', { name: 'Stop', exact: true })).toBeEnabled();
  await card.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(card.getByLabel('Secret value', { exact: true })).toHaveValue('');
  const responses = await app.evaluate(() => (globalThis as typeof globalThis & { questionResponses: { response: unknown }[] }).questionResponses);
  expect(responses.map(request => request.response)).toEqual([{ action: 'stop', answers: [] }]);
});

test('a report question provides a secret without routing its value through a reply', async () => {
  const page = await app.firstWindow();
  const fixture = questionFixture();
  const state = { ...fixture, workers: [{ ...fixture.workers[0], prompts: [], activeTurn: null, checkpoint: { turnId: 'report-turn', sha: 'a'.repeat(40), reference: 'checkpoint', unmergedPaths: [], liveProcesses: false } }], reportQuestions: [{ id: 'report-question', reportId: 'report', trackId: 'A', sessionId: 'session', turnId: 'report-turn', revision: 4, kind: 'question', text: 'Provide DEMO_TOKEN to continue.', state: 'open', replyUtteranceId: null }] };
  await app.evaluate(({ ipcMain, BrowserWindow }, state) => {
    const shared = globalThis as typeof globalThis & { reportSecrets: unknown[]; reportAnswers: unknown[] };
    shared.reportSecrets = []; shared.reportAnswers = [];
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:worker-secret'); ipcMain.handle('intercat:worker-secret', (_event, request) => { shared.reportSecrets.push(request); return { accepted: true }; });
    ipcMain.removeHandler('intercat:report-response'); ipcMain.handle('intercat:report-response', (_event, request) => { shared.reportAnswers.push(request); });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, state);
  const card = page.getByRole('group', { name: 'Question from Worker A', exact: true });
  await card.getByRole('button', { name: 'Provide a secret', exact: true }).click();
  await card.getByRole('radio', { name: 'This time only', exact: true }).check();
  await card.getByLabel('Secret variable name', { exact: true }).fill('DEMO_TOKEN');
  await card.getByLabel('Secret value', { exact: true }).fill('dummy-report-secret');
  await card.getByLabel('Secret value', { exact: true }).scrollIntoViewIfNeeded();
  const valueBox = await card.getByLabel('Secret value', { exact: true }).boundingBox();
  const choicesBox = await card.locator('.secret-footer').boundingBox();
  expect(valueBox!.y + valueBox!.height).toBeLessThanOrEqual(choicesBox!.y);
  const scope = card.locator('.secret-scope');
  await expect(scope).toBeInViewport();
  await page.screenshot({ path: 'research/verify/report-secret-20261104/report-form.png' });
  await card.getByRole('button', { name: 'Use for this worker', exact: true }).click();
  const requests = await app.evaluate(() => { const shared = globalThis as typeof globalThis & { reportSecrets: unknown[]; reportAnswers: unknown[] }; return { reportSecrets: shared.reportSecrets, reportAnswers: shared.reportAnswers }; });
  expect(requests.reportSecrets).toEqual([expect.objectContaining({ trackId: 'A', sessionId: 'session', turnId: 'report-turn', name: 'DEMO_TOKEN', value: 'dummy-report-secret', question: { kind: 'report', id: 'report-question', revision: 4 } })]);
  expect(requests.reportAnswers).toEqual([]);
});

test('file recovery can be cancelled while pending and cancellation acknowledgements can be retried', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const state = { connected: true, workerConfigured: true, sequence: 1, tracks: [{ id: 'recover-cancel', label: 'A', title: 'Interrupted work', status: 'waiting' }], utterances: [], pickups: [] as { opId: string; trackId: string; sessionId: string; phase: string; snapshotSha: string }[], workers: [{ trackId: 'recover-cancel', phase: 'lost', sessionId: 'old-session', identity: null, detail: 'The old session cannot be resumed.', activeTurn: null, checkpointPending: false, checkpoint: null, prompts: [], output: null, approvals: [] }] };
    const calls = { recover: [] as Record<string, string>[], cancel: [] as Record<string, string>[] };
    (globalThis as typeof globalThis & { cancelCalls: typeof calls }).cancelCalls = calls;
    let interrupt: ((error: Error) => void) | undefined;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:pickup'); ipcMain.handle('intercat:pickup', (_event, request) => {
      calls.recover.push(request); state.pickups.push({ ...request, phase: 'summarizing', snapshotSha: 'saved' }); state.workers[0].phase = 'after-connection-loss'; state.sequence++;
      return new Promise((_resolve, reject) => { interrupt = reject; });
    });
    ipcMain.removeHandler('intercat:pickup-cancel'); ipcMain.handle('intercat:pickup-cancel', (_event, request) => {
      calls.cancel.push(request); interrupt?.(new Error('Recovery interrupted')); state.pickups.at(-1)!.phase = 'cancelling'; state.sequence++;
      if (calls.cancel.length === 1) { state.pickups.at(-1)!.phase = 'cancelled'; throw new Error('Cancellation acknowledgement lost'); }
      state.pickups.at(-1)!.phase = 'cancelled'; state.sequence++; return state.pickups.at(-1);
    });
    BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Pick up from files', exact: true }).click();
  await page.getByRole('button', { name: 'Cancel recovery', exact: true }).click();
  await expect(page.getByRole('alert').filter({ hasText: 'Cancellation acknowledgement lost' })).toBeVisible();
  await page.getByRole('button', { name: 'Retry cancellation', exact: true }).click();
  await expect(page.getByText('Recovery cancelled. Files and saved snapshots were kept.', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: 'Pick up from files', exact: true }).click();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { cancelCalls: { recover: Record<string, string>[]; cancel: Record<string, string>[] } }).cancelCalls);
  expect(calls.cancel).toHaveLength(2); expect(calls.cancel[0]).toEqual(calls.cancel[1]);
  expect(calls.cancel[0].recoveryId).toBe(calls.recover[0].opId);
  expect(calls.recover).toHaveLength(2); expect(calls.recover[1].opId).not.toBe(calls.recover[0].opId);
});

test('a persisted cancellation can finish after the app reopens', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const recovery = { opId: 'saved-recovery', trackId: 'cancel-restart', sessionId: 'retained-session', phase: 'cancelling', snapshotSha: 'saved' };
    const state = { connected: true, workerConfigured: true, sequence: 1, tracks: [{ id: recovery.trackId, label: 'A', title: 'Interrupted work', status: 'waiting' }], utterances: [], pickups: [recovery], workers: [{ trackId: recovery.trackId, phase: 'after-connection-loss', sessionId: recovery.sessionId, identity: null, detail: 'The old worker stopped.', activeTurn: null, checkpointPending: false, checkpoint: null, prompts: [], output: null, approvals: [] }] };
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:pickup-cancel'); ipcMain.handle('intercat:pickup-cancel', (_event, request) => {
      if (request.recoveryId !== recovery.opId || request.sessionId !== recovery.sessionId) throw new Error('Wrong recovery');
      recovery.phase = 'cancelled'; state.sequence++; return recovery;
    });
    BrowserWindow.getAllWindows()[0].reload();
  });
  await expect(page.getByRole('button', { name: 'Cancelling recovery…', exact: true })).toBeDisabled();
  await page.getByRole('button', { name: 'Retry cancellation', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Pick up from files', exact: true })).toBeEnabled();
});

test('blocked backup review hides history and retries a lost process-stop acknowledgement', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const review = { historyPaused: true, sessions: [], pendingOperations: 2, missingProcessRecords: ['work/missing-track'], workspaces: [{ trackId: 'backup-track', sessionId: 'old-session', path: '/dummy/worktree', savedReference: { matches: false }, filesReviewed: false, attachmentsReviewed: false, lastReviewId: null }], hosts: [{ id: 'summary/unlogged/host', workspace: '/dummy/worktree', root: { pid: 12345, started_seconds: 123, started_micros: 0 }, processes: 1, groups: 1, knownToLog: false, issue: null }] };
    const calls: unknown[] = []; (globalThis as typeof globalThis & { backupStopCalls: unknown[] }).backupStopCalls = calls;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: false, mode: 'unavailable', message: 'History remains paused.', fingerprintRecovery: { sessions: [{ trackId: 'backup-track', sessionId: 'old-session', reason: 'missing' }], projects: [], historyReviewRequired: true } }));
    ipcMain.removeHandler('intercat:backup-review'); ipcMain.handle('intercat:backup-review', (_event, request) => {
      if (request.action === 'inspect') return review;
      calls.push(request);
      if (calls.length === 1) throw new Error('Stop acknowledgement lost');
      review.hosts[0].processes = 0; review.hosts[0].groups = 0; return review;
    });
    BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Review saved files and processes', exact: true }).click();
  const review = page.getByRole('region', { name: 'Saved recovery records' });
  await expect(review).toContainText('Saved reference differs or is unavailable.');
  await expect(review).toContainText('This host is absent from the restored operation log.');
  await expect(review).toContainText('Process ownership records are missing');
  const stop = review.getByRole('button', { name: 'Stop recorded processes for PID 12345', exact: true });
  await stop.click(); await expect(page.getByRole('alert').filter({ hasText: 'Stop acknowledgement lost' }).last()).toBeVisible();
  await stop.click(); await expect(review).toContainText('0 owned process(es)');
  await expect(review).toContainText('History remains paused.');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { backupStopCalls: unknown[] }).backupStopCalls);
  expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
  await expect(page.getByRole('button', { name: 'Retry worker', exact: true })).toHaveCount(0);
});

test('backup file comparison retries its receipt and requires a new review after files change', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const base = { historyPaused: true, sessions: [], pendingOperations: 0, missingProcessRecords: [], workspaces: [{ trackId: 'files', sessionId: 'old', path: '/dummy/worktree', savedReference: { matches: false }, filesReviewed: false, attachmentsReviewed: false, lastReviewId: null }], hosts: [] };
    const calls: Record<string, unknown>[] = []; (globalThis as typeof globalThis & { fileReviewCalls: typeof calls }).fileReviewCalls = calls;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: false, mode: 'unavailable', message: 'History remains paused.', fingerprintRecovery: { sessions: [{ trackId: 'files', sessionId: 'old', reason: 'missing' }], projects: [], historyReviewRequired: true } }));
    ipcMain.removeHandler('intercat:backup-review'); ipcMain.handle('intercat:backup-review', (_event, request) => {
      if (request.action === 'inspect') return base;
      if (request.action !== 'reviewFiles') throw new Error('Unexpected action');
      calls.push(request);
      if (calls.length === 1) throw new Error('Review acknowledgement lost');
      if (calls.length === 4) return { ...base, workspaces: [{ ...base.workspaces[0], attachmentsReviewed: true }], fileReview: { opId: request.opId, trackId: 'files', baseline: 'a'.repeat(40), seal: 'b'.repeat(64), files: 3, directories: 1, changes: [], protected: [], unsupported: [], attachments: { originals: 2, verifiedOriginals: 2, missingOriginals: [], invalidOriginals: [], unrecordedOriginals: [], copies: [{ path: '.intercat/attachments/reference.txt', attachmentId: 'reference', state: 'matched' }], complete: true }, complete: true, stale: false } };
      return { ...base, fileReview: { opId: request.opId, trackId: 'files', baseline: 'a'.repeat(40), seal: 'b'.repeat(64), files: 3, directories: 1, changes: [{ path: 'code.txt', kind: 'modified' }], protected: ['.env'], unsupported: [], attachments: { originals: 2, verifiedOriginals: 1, missingOriginals: ['missing-original'], invalidOriginals: [], unrecordedOriginals: ['newer-original'], copies: [{ path: '.intercat/attachments/copy.txt', attachmentId: null, state: 'unrecorded' }], complete: false }, complete: false, stale: calls.length === 2 } };
    });
    BrowserWindow.getAllWindows()[0].setSize(1440, 1000);
    BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2);
    BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Review saved files and processes', exact: true }).click();
  const compare = page.getByRole('button', { name: 'Compare files and originals', exact: true });
  await compare.click(); await expect(page.getByRole('alert').filter({ hasText: 'Review acknowledgement lost' }).last()).toBeVisible();
  await compare.click(); await expect(page.getByText(/Files or originals changed after this review/)).toBeVisible();
  await compare.click(); await expect(page.getByText(/Files or originals changed after this review/)).toHaveCount(0);
  await expect(page.getByText(/Protected files were left unread: .env/)).toBeVisible();
  await expect(page.getByText(/Copy needs review:/)).toBeVisible();
  await expect(page.getByText('Missing originals: missing-original', { exact: true })).toBeVisible();
  await expect(page.getByText('Original files absent from the restored log: newer-original', { exact: true })).toBeVisible();
  await page.getByText(/Copy needs review:/).scrollIntoViewIfNeeded();
  await expect(page.getByText(/Copy needs review:/)).toBeInViewport();
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  const image = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/backup-attachments-20261104/review-ui.png', Buffer.from(image, 'base64'));
  await compare.click();
  await expect(page.getByText(/Recorded originals and workspace copies compared/)).toBeVisible();
  await expect(page.getByText('Verified originals: 2 / 2. Matching workspace copies: 1.', { exact: true })).toBeVisible();
  await expect(page.getByText(/Copy needs review:/)).toHaveCount(0);
  await expect(page.getByText('b'.repeat(64), { exact: true })).toHaveCount(0);
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { fileReviewCalls: Record<string, unknown>[] }).fileReviewCalls);
  expect(calls).toHaveLength(4); expect(calls[0]).toEqual(calls[1]); expect(calls[2].opId).not.toBe(calls[1].opId); expect(calls[3].opId).not.toBe(calls[2].opId);
});

test('backup session choices survive a lost acknowledgement and remain pending after reload', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    let choice: unknown = null;
    const calls: Record<string, unknown>[] = [];
    (globalThis as typeof globalThis & { backupChoiceCalls: typeof calls }).backupChoiceCalls = calls;
    const base = () => ({ historyPaused: true, pendingOperations: 0, missingProcessRecords: [], sessions: [{ sessionId: 'lost', trackId: 'track', reason: 'missing', canPickUp: true, choice }], workspaces: [{ trackId: 'track', sessionId: 'lost', path: '/dummy/worktree', savedReference: { matches: false }, filesReviewed: false, attachmentsReviewed: false, lastReviewId: null }], hosts: [] });
    let fileReview: unknown;
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: false, mode: 'unavailable', message: 'History remains paused.', fingerprintRecovery: { sessions: [{ sessionId: 'lost', trackId: 'track', reason: 'missing' }], projects: [], historyReviewRequired: true } }));
    ipcMain.removeHandler('intercat:backup-review'); ipcMain.handle('intercat:backup-review', (_event, request) => {
      if (request.action === 'inspect') return base();
      if (request.action === 'reviewFiles') {
        fileReview = { opId: request.opId, trackId: 'track', baseline: 'a'.repeat(40), seal: 'b'.repeat(64), files: 1, directories: 0, changes: [], protected: [], unsupported: [], attachments: { originals: 0, verifiedOriginals: 0, missingOriginals: [], invalidOriginals: [], unrecordedOriginals: [], copies: [], complete: true }, complete: true, stale: false };
        return { ...base(), fileReview };
      }
      if (request.action !== 'chooseSession') throw new Error('Unexpected request');
      calls.push(request);
      choice = { opId: request.opId, choice: request.choice, reviewId: request.reviewId, acknowledgeUnread: request.acknowledgeUnread, needsRecheck: true, stale: false };
      if (calls.length === 1) throw new Error('Choice acknowledgement lost');
      return { ...base(), fileReview };
    });
    BrowserWindow.getAllWindows()[0].setSize(1440, 1000); BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2);
    BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button', { name: 'Review saved files and processes', exact: true }).click();
  const pickup = page.getByRole('button', { name: 'Save Pick up from files', exact: true });
  await expect(pickup).toBeDisabled();
  await page.getByRole('button', { name: 'Compare files and originals', exact: true }).click();
  await pickup.click(); await expect(page.getByRole('alert').filter({ hasText: 'Choice acknowledgement lost' }).last()).toBeVisible();
  await pickup.click(); await expect(page.getByText(/Saved choice: Pick up from files/)).toBeVisible();
  await page.reload(); await page.getByRole('button', { name: 'Review saved files and processes', exact: true }).click();
  await expect(page.getByText(/Saved choice: Pick up from files/)).toBeVisible();
  await expect(pickup).toBeDisabled();
  await page.getByRole('button', { name: 'Compare files and originals', exact: true }).click();
  await page.getByRole('button', { name: 'Save Discard history', exact: true }).click();
  const saved = page.getByText(/Saved choice: Discard history/);
  await expect(saved).toBeVisible(); await saved.scrollIntoViewIfNeeded(); await expect(saved).toBeInViewport();
  await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
  const pixels=await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/backup-choices-20261104/recovery-choices.png', Buffer.from(pixels, 'base64'));
  const calls=await app.evaluate(() => (globalThis as typeof globalThis & { backupChoiceCalls: Record<string, unknown>[] }).backupChoiceCalls);
  expect(calls).toHaveLength(3); expect(calls[0]).toEqual(calls[1]); expect(calls[2].choice).toBe('discard-history'); expect(calls[2].opId).not.toBe(calls[1].opId);
  await expect(page.getByText('Choose how to recover each affected session. Saving a choice keeps history paused.', { exact: true })).toBeVisible();
});

test('an unmatched restored session cannot borrow another sessions file review', async () => {
  const page=await app.firstWindow();
  await app.evaluate(({ ipcMain,BrowserWindow }) => {
    const base={ historyPaused:true,pendingOperations:0,missingProcessRecords:[],hosts:[],sessions:[{sessionId:'missing',trackId:'track',reason:'missing',canPickUp:false,choice:null as unknown}],workspaces:[{trackId:'track',sessionId:'retained',path:'/dummy/current',savedReference:{matches:true},filesReviewed:true,attachmentsReviewed:true,lastReviewId:null}],fileReview:{opId:'01a1a200-0000-7000-8000-000000000099',trackId:'track',baseline:'a'.repeat(40),seal:'b'.repeat(64),files:0,directories:0,changes:[],protected:[],unsupported:[],attachments:{originals:0,verifiedOriginals:0,missingOriginals:[],invalidOriginals:[],unrecordedOriginals:[],copies:[],complete:true},complete:true,stale:false} };
    ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>({connected:false,mode:'unavailable',message:'Paused',fingerprintRecovery:{sessions:[{sessionId:'missing',trackId:'track',reason:'missing'}],projects:[],historyReviewRequired:true}}));
    ipcMain.removeHandler('intercat:backup-review');ipcMain.handle('intercat:backup-review',(_event,request)=>{
      if(request.action==='inspect')return base;
      if(request.action!=='chooseSession'||request.choice!=='discard-history'||request.sessionId!=='missing'||request.reviewId!==null||request.acknowledgeUnread!==true)throw new Error('Wrong session review');
      base.sessions[0].choice={opId:request.opId,choice:request.choice,reviewId:null,acknowledgeUnread:true,needsRecheck:true,stale:false};return base;
    });BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button',{name:'Review saved files and processes',exact:true}).click();
  await expect(page.getByRole('button',{name:'Save Pick up from files',exact:true})).toBeDisabled();
  const discard=page.getByRole('button',{name:'Save Discard history',exact:true});await expect(discard).toBeDisabled();
  await page.getByRole('checkbox',{name:'I understand that these files and originals are unverified and will be kept.'}).check();
  await discard.click();await expect(page.getByText(/Saved choice: Discard history/)).toBeVisible();
});

test('prepared backup recovery retries its original operation before reopening the workspace', async () => {
  const page=await app.firstWindow();
  await app.evaluate(({ipcMain,BrowserWindow})=>{
    const pending={opId:'01a1a200-0000-7000-8000-000000000078',phase:'awaiting-receipt',acknowledgeUnverified:true};
    let complete=false;
    const calls:Record<string,unknown>[]=[];(globalThis as typeof globalThis & {backupApplyCalls:typeof calls}).backupApplyCalls=calls;
    ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>complete?{connected:true,sequence:1,tracks:[],workers:[],utterances:[]}:{connected:false,mode:'unavailable',message:'Recovery confirmation pending',fingerprintRecovery:{sessions:[],projects:[],historyReviewRequired:true,pendingApply:pending}});
    ipcMain.removeHandler('intercat:backup-apply');ipcMain.handle('intercat:backup-apply',(_event,request)=>{
      calls.push(request);if(calls.length===1)throw new Error('Confirmation acknowledgement lost');
      complete=true;
      return {connected:true,sequence:1,tracks:[],workers:[],utterances:[]};
    });BrowserWindow.getAllWindows()[0].reload();
  });
  const finish=page.getByRole('button',{name:'Finish prepared recovery',exact:true});
  await finish.click();await expect(page.getByRole('alert').filter({hasText:'Confirmation acknowledgement lost'}).last()).toBeVisible();
  await finish.click();await expect(finish).toHaveCount(0);
  const calls=await app.evaluate(()=>(globalThis as typeof globalThis & {backupApplyCalls:Record<string,unknown>[]}).backupApplyCalls);
  expect(calls).toHaveLength(2);expect(calls[0]).toEqual(calls[1]);expect(calls[0]).toEqual({opId:'01a1a200-0000-7000-8000-000000000078',acknowledgeUnverified:true});
});

test('reviewed backup choices can be applied only after all workspace reviews are present', async () => {
  const page=await app.firstWindow();
  await app.evaluate(({ipcMain,BrowserWindow})=>{
    let reviewed=false;let complete=false;
    const state=()=>complete?{connected:true,sequence:1,tracks:[],workers:[],utterances:[]}:{connected:false,mode:'unavailable',message:'History remains paused.',fingerprintRecovery:{sessions:[{sessionId:'old',trackId:'track',reason:'missing'}],projects:[],historyReviewRequired:true}};
    ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',state);
    ipcMain.removeHandler('intercat:backup-review');ipcMain.handle('intercat:backup-review',(_event,request)=>{
      if(request.action==='reviewFiles')reviewed=true;
      return {historyPaused:true,pendingOperations:0,missingProcessRecords:[],hosts:[],sessions:[{sessionId:'old',trackId:'track',reason:'missing',canPickUp:true,choice:{opId:'saved',choice:'pick-up',reviewId:'review',acknowledgeUnread:false,needsRecheck:true,stale:false}}],workspaces:[{trackId:'track',sessionId:'old',path:'/dummy/worker',savedReference:{matches:true},filesReviewed:reviewed,attachmentsReviewed:reviewed,lastReviewId:reviewed?'review':null}]};
    });
    ipcMain.removeHandler('intercat:backup-apply');ipcMain.handle('intercat:backup-apply',(_event,request)=>{
      if(!reviewed||request.acknowledgeUnverified!==false||!/^.{8}-.{4}-7/.test(request.opId))throw new Error('Missing recovery review');
      complete=true;return state();
    });BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button',{name:'Review saved files and processes',exact:true}).click();
  const apply=page.getByRole('button',{name:'Apply reviewed recovery',exact:true});await expect(apply).toBeDisabled();
  await page.getByRole('button',{name:'Compare files and originals',exact:true}).click();await expect(apply).toBeEnabled();
  await apply.click();await expect(apply).toHaveCount(0);
});

test('unfinished backup preparation can stop owned processes and return to review after cancellation', async () => {
  const page=await app.firstWindow();
  await app.evaluate(({ipcMain,BrowserWindow})=>{
    let cancelled=false,stopped=false;
    const pending={opId:'01a1a200-0000-7000-8000-000000000079',phase:'capturing',acknowledgeUnverified:false};
    const state=()=>({connected:false,mode:'unavailable',message:'Recovery preparation pending',fingerprintRecovery:{sessions:[],projects:[],historyReviewRequired:true,...(cancelled?{}:{pendingApply:pending})}});
    const calls:Record<string,unknown>[]=[];(globalThis as typeof globalThis & {backupCancelCalls:typeof calls}).backupCancelCalls=calls;
    ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',state);
    ipcMain.removeHandler('intercat:backup-review');ipcMain.handle('intercat:backup-review',(_event,request)=>{
      if(request.action==='stop')stopped=true;
      return {historyPaused:true,...(cancelled?{}:{apply:pending}),pendingOperations:cancelled?0:1,missingProcessRecords:[],sessions:[],workspaces:[{trackId:'track',sessionId:'old',path:'/dummy/worker',savedReference:{matches:true},lastReviewId:'review'}],hosts:[{id:'work/track/host',root:{pid:8123,started_seconds:1,started_micros:2},workspace:'/dummy/worker',processes:stopped?0:1,groups:stopped?0:1,issue:null,knownToLog:true}]};
    });
    ipcMain.removeHandler('intercat:backup-cancel');ipcMain.handle('intercat:backup-cancel',(_event,request)=>{calls.push(request);cancelled=true;return state();});
    BrowserWindow.getAllWindows()[0].setSize(1440,1000);BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2);BrowserWindow.getAllWindows()[0].reload();
  });
  await page.getByRole('button',{name:'Review saved files and processes',exact:true}).click();
  await expect(page.getByRole('button',{name:'Compare files and originals',exact:true})).toBeDisabled();
  const stop=page.getByRole('button',{name:'Stop recorded processes for PID 8123',exact:true});
  await stop.click();await expect(stop).toHaveCount(0);
  const cancel=page.getByRole('button',{name:'Cancel preparation and review again',exact:true});
  await cancel.scrollIntoViewIfNeeded();await expect(cancel).toBeInViewport();
  const pixels=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/backup-apply-20261104/prepared-recovery.png',Buffer.from(pixels,'base64'));
  await cancel.click();await expect(cancel).toHaveCount(0);
  await page.getByRole('button',{name:'Review saved files and processes',exact:true}).click();
  await expect(page.getByRole('button',{name:'Compare files and originals',exact:true})).toBeEnabled();
  const calls=await app.evaluate(()=>(globalThis as typeof globalThis & {backupCancelCalls:Record<string,unknown>[]}).backupCancelCalls);
  expect(calls).toEqual([{opId:'01a1a200-0000-7000-8000-000000000079'}]);
});
