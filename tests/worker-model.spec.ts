import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import type { DaemonStatus } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

const execute = promisify(execFile);
async function capture(app: ElectronApplication, name: string) {
  await (await app.firstWindow()).evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath(name), Buffer.from(pixels, 'base64'));
}

test.describe('observed worker model presentation', () => {
  let app: ElectronApplication;
  let data: string;
  test.beforeEach(async () => {
    data = await mkdtemp('/private/tmp/ic-worker-model-');
    app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const now = Date.now();
      const state: DaemonStatus = { connected: true, workerConfigured: true, sequence: 1,
        tracks: [{ id: 'A', label: 'A', title: 'First task', status: 'working' }, { id: 'B', label: 'B', title: 'Second task', status: 'working' }],
        workers: ['A', 'B'].map(trackId => ({ trackId, sessionId: trackId, phase: 'working', activeTurn: null, detail: null, identity: null, output: null, approvals: [], prompts: [], checkpointPending: false, checkpoint: null,
          model: { modelId: trackId === 'A' ? 'muse-spark-1.3-contributor' : 'muse-wonder', providerId: 'meta', reasoningEffort: trackId === 'A' ? 'minimal' : 'high' } })),
        utterances: [], timeline: { summaries: [], clock: { epochMs: now, monoMs: 0, timelineMs: now, bootId: 'fixture' } } };
      const clip = { id: 'step-a', trackId: 'A', ordinal: 1, sessionId: 'A', turnId: 'turn-a', category: 'step', label: 'Reply', kind: 'think', start: now - 120000, end: now - 30000, running: false, generationEnd: null, approximate: false, toolTicks: [] };
      Object.assign(globalThis, { modelFixture: { state, clip } });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [clip], removed: [], marks: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:clip-detail'); ipcMain.handle('intercat:clip-detail', () => ({ clip, model: state.workers![0].model, items: [] }));
    });
  });
  test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });

  test('the locked destination and viewed transcript keep their own model at both zoom levels', async () => {
    const page = await app.firstWindow();
    const status = page.getByLabel('Request model', { exact: true });
    await page.getByRole('gridcell', { name: /^A ·/ }).click();
    await expect(status).toHaveText('Worker A · Spark 1.3 · minimal');
    const target = page.getByRole('combobox', { name: 'Request destination' });
    await target.selectOption('B');
    await expect(status).toHaveText('Only B · Wonder · high');
    await page.getByRole('button', { name: 'Open step A:1: Reply', exact: true }).focus();
    await page.keyboard.press('Enter');
    await expect(page.locator('.step-metadata .worker-model')).toHaveText('Spark 1.3 · minimal');
    const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill('Continue the second task');
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      await status.scrollIntoViewIfNeeded();
      await expect(status).toBeInViewport();
      await expect(page.getByRole('button', { name: 'Put', exact: true })).toBeInViewport();
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      const statusBox = (await status.boundingBox())!;
      const composer = (await page.locator('.composer').boundingBox())!;
      expect(statusBox.y).toBeGreaterThanOrEqual(composer.y + composer.height);
      const label = (await status.locator('.worker-model').boundingBox())!;
      expect(Math.abs(label.x + label.width - statusBox.x - statusBox.width)).toBeLessThan(2);
      await capture(app, `locked-b-view-a-${zoom}.png`);
      const transcriptModel = page.locator('.step-metadata .worker-model');
      await transcriptModel.scrollIntoViewIfNeeded();
      await expect(transcriptModel).toBeInViewport();
      await expect(transcriptModel).toHaveText('Spark 1.3 · minimal');
      await capture(app, `viewed-transcript-a-${zoom}.png`);
    }
    await target.selectOption('');
    await expect(status).toHaveText('Worker A · Spark 1.3 · minimal');
    await page.getByRole('gridcell', { name: /^B ·/ }).click();
    await expect(status).toHaveText('Worker B · Wonder · high');
  });

  test('live changes, missing values and a vanished lock never borrow another worker settings', async () => {
    const page = await app.firstWindow();
    await page.getByRole('gridcell', { name: /^A ·/ }).click();
    const status = page.getByLabel('Request model', { exact: true });
    await page.getByRole('combobox', { name: 'Request destination' }).selectOption('B');
    await app.evaluate(() => {
      const { state } = (globalThis as typeof globalThis & { modelFixture: { state: DaemonStatus } }).modelFixture;
      state.workers![1].model = { modelId: 'future-model-v2', providerId: null, reasoningEffort: null };
      state.sequence!++;
    });
    await expect(status).toHaveText('Only B · future-model-v2 · effort —');
    await app.evaluate(() => {
      const { state } = (globalThis as typeof globalThis & { modelFixture: { state: DaemonStatus } }).modelFixture;
      state.workers![1].model = null; state.sequence!++;
    });
    await expect(status).toHaveText('Only B · Model — · effort —');
    await app.evaluate(() => {
      const { state } = (globalThis as typeof globalThis & { modelFixture: { state: DaemonStatus } }).modelFixture;
      state.tracks = state.tracks!.slice(0, 1); state.workers = state.workers!.slice(0, 1); state.sequence!++;
    });
    await expect(status).toHaveText('Only unavailable track · Model — · effort —');
    await page.getByRole('combobox', { name: 'Request destination' }).selectOption('');
    await expect(status).toHaveText('Worker A · Spark 1.3 · minimal');
    await app.evaluate(() => {
      const { state } = (globalThis as typeof globalThis & { modelFixture: { state: DaemonStatus } }).modelFixture;
      state.tracks = []; state.workers = []; state.sequence!++;
    });
    await expect(status).toHaveText('Editor picks per track');
  });

  test('a repair displays its own session settings while another track is viewed', async () => {
    const page = await app.firstWindow();
    await app.evaluate(() => {
      const { state } = (globalThis as typeof globalThis & { modelFixture: { state: DaemonStatus } }).modelFixture;
      state.repairRuns = [{ id: 'repair-b', owner:{kind:'commit',id:'job',revision:1}, revision: 1, track: 'B', purpose: { kind: 'check' }, phase: 'held',
        approvals: [], prompts: [], output: null, failure: null, session: 'repair-session', refresh: null, remaining: null, restoring: null, turn: null, secretInput: null,
        model: { modelId: 'repair-model', providerId: null, reasoningEffort: 'medium' } }];
      state.sequence!++;
    });
    await page.getByRole('gridcell', { name: /^B · for the commit/ }).click();
    const status = page.getByLabel('Request model', { exact: true });
    await expect(status).toHaveText('Worker B · for the commit · repair-model · medium');
    await page.getByRole('gridcell', { name: /^A · First/ }).click();
    await expect(status).toHaveText('Worker B · for the commit · repair-model · medium');
    await app.evaluate(() => {
      const { state } = (globalThis as typeof globalThis & { modelFixture: { state: DaemonStatus } }).modelFixture;
      state.repairRuns![0].model = null; state.sequence!++;
    });
    await expect(status).toHaveText('Worker B · for the commit · Model — · effort —');
    await page.getByRole('button', { name: 'Clear repair target', exact: true }).click();
    await expect(status).toHaveText('Worker A · Spark 1.3 · minimal');
  });

  for (const zoom of [1,2]) test(`the paired first effort stays distinct from defaults and later work at ${zoom*100}%`,async()=>{
    const page=await app.firstWindow();
    await app.evaluate(({BrowserWindow},zoom)=>{const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);},zoom);
    await app.evaluate(()=>{
      const {state}=(globalThis as any).modelFixture;
      const worker=state.workers[1];worker.phase='ready';worker.activeTurn=null;
      worker.model={modelId:'muse-spark-1.3-contributor',providerId:'meta',reasoningEffort:'minimal'};
      worker.pairedInstruction={inputId:'own-paired-word',effort:'xhigh',phase:'pending'};state.sequence++;
    });
    const card=page.locator('[data-worker-card="B"]');
    await expect(card.locator('.worker-model')).toHaveText('Spark 1.3 · default minimal');
    await expect(card.locator('.worker-paired-effort')).toHaveText('Two-way instruction: xhigh · queued');
    const model=card.locator('.worker-models');await model.scrollIntoViewIfNeeded();
    await expect(model).toBeInViewport();
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    const start=card.getByRole('button',{name:'Start Worker B',exact:true});await start.focus();await expect(start).toBeFocused();
    expect(await start.evaluate(element=>{const rect=element.getBoundingClientRect();return element.contains(document.elementFromPoint(rect.left+rect.width/2,rect.top+rect.height/2));})).toBe(true);
    const stop=card.getByRole('button',{name:'Stop Worker B',exact:true});
    const startRect=(await start.boundingBox())!,stopRect=(await stop.boundingBox())!;
    expect(stopRect.x-startRect.x-startRect.width).toBeLessThanOrEqual(8);
    await model.scrollIntoViewIfNeeded();await capture(app,`paired-pending-${zoom}.png`);
    await app.evaluate(()=>{const {state}=(globalThis as any).modelFixture;const worker=state.workers[1];worker.activeTurn='own-paired-turn';worker.phase='working';worker.model.reasoningEffort='xhigh';worker.pairedInstruction.phase='active';state.sequence++;});
    await expect(card.locator('.worker-model')).toHaveText('Spark 1.3 · xhigh');
    await expect(card.locator('.worker-paired-effort')).toHaveText('Two-way instruction: xhigh');
    await app.evaluate(()=>{const {state}=(globalThis as any).modelFixture;const worker=state.workers[1];worker.activeTurn=null;worker.phase='ready';worker.model.reasoningEffort='minimal';worker.pairedInstruction.phase='finished';state.sequence++;});
    await expect(card.locator('.worker-model')).toHaveText('Spark 1.3 · default minimal');
    await expect(card.locator('.worker-paired-effort')).toHaveText('Two-way instruction: xhigh · finished');
    await model.scrollIntoViewIfNeeded();await capture(app,`paired-finished-${zoom}.png`);
    await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
    await expect(page.getByLabel('Request model',{exact:true})).toHaveText('Only B · Spark 1.3 · minimal');
    await app.evaluate(()=>{const {state}=(globalThis as any).modelFixture;const worker=state.workers[1];worker.activeTurn='own-next-turn';worker.phase='working';state.sequence++;});
    await expect(card.locator('.worker-model')).toHaveText('Spark 1.3 · minimal');
    await expect(card.locator('.worker-paired-effort')).toHaveText('Two-way instruction: xhigh · finished');
    await app.evaluate(()=>{const {state}=(globalThis as any).modelFixture;state.workers[1].model.modelId='own-'+('future-model-'.repeat(32));state.sequence++;});
    await expect(card.locator('.worker-model')).toContainText('own-future-model-');
    expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
    await stop.focus();await expect(stop).toBeFocused();
    expect(await stop.evaluate(element=>{const rect=element.getBoundingClientRect();return element.contains(document.elementFromPoint(rect.left+rect.width/2,rect.top+rect.height/2));})).toBe(true);
    await app.evaluate(()=>{const {state}=(globalThis as any).modelFixture;state.workers[1].pairedInstruction=null;state.sequence++;});
    await expect(card.locator('.worker-paired-effort')).toHaveCount(0);
  });
});

test('native saved session metadata and a locked draft survive daemon restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180000);
  const data = await mkdtemp('/private/tmp/ic-native-model-');
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  const stop = async () => {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have exited. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false);
      pid = undefined;
    }
  };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await stop();
    const ids = ['f8106fda-e424-4c82-8713-0197fbabda1c', 'ec8be407-a15a-42a8-a7f6-bab08707cbb2'];
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys,time
db=sqlite3.connect(sys.argv[1]); ids=json.loads(sys.argv[2]); now=int(time.time()*1000)
for i,track in enumerate(ids):
 session='saved-model-'+str(i)
 db.execute('INSERT INTO tracks VALUES(?,?,?,?)',(track,'AB'[i],'Saved task '+str(i),'waiting'))
 worker={'trackId':track,'phase':'held','sessionId':session,'secretRevision':None,'identity':None,'detail':None,'activeTurn':None,'approvals':[],'output':None,'checkpointPending':False,'checkpoint':None,'prompts':[]}
 db.execute('INSERT INTO workers VALUES(?,?)',(track,json.dumps(worker)))
 for method,fields in [('session/modelChanged',{'modelId':'muse-spark-1.3-contributor' if i==0 else 'muse-wonder','providerId':'meta','source':'startup'}),('session/reasoningEffortChanged',{'reasoningEffort':'minimal' if i==0 else 'high','source':'startup'})]:
  cursor=session+method; fields.update({'sessionId':session,'viewCursor':cursor,'sourceRange':{}}); payload=json.dumps({'jsonrpc':'2.0','method':method,'params':fields})
  raw=db.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload) VALUES(?,0,'fixture',?,?,?,'msp.received',?)",(now,now,track,session,payload)).lastrowid
  applied=db.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?,0,'fixture',?,?,?,'msp.applied',?,?)",(now,now,track,session,payload,raw)).lastrowid
  db.execute('INSERT INTO msp_applied VALUES(?,?,?)',(session,cursor,applied))
db.commit(); db.close()`, path.join(data, 'daemon/events.sqlite'), JSON.stringify(ids)]);
    for (let restart = 0; restart < 2; restart++) {
      app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
      page = await app.firstWindow();
      await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
      const saved = await page.evaluate(() => window.intercat.getStatus());
      pid = saved.daemonPid;
      expect(saved.workers?.map(worker => worker.model)).toEqual([
        { modelId: 'muse-spark-1.3-contributor', providerId: 'meta', reasoningEffort: 'minimal' },
        { modelId: 'muse-wonder', providerId: 'meta', reasoningEffort: 'high' },
      ]);
      await page.getByRole('gridcell', { name: /^A ·/ }).click();
      if (restart === 0) {
        await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Continue B after review');
        await page.getByRole('combobox', { name: 'Request destination' }).selectOption(ids[1]);
      }
      await expect(page.getByLabel('Request model', { exact: true })).toHaveText('Only B · Wonder · high');
      await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Continue B after review');
      await capture(app, `native-restored-${restart}.png`);
      await writeFile(test.info().outputPath(`native-restored-${restart}.json`), JSON.stringify(saved, null, 2));
      await stop();
    }
  } finally { await stop(); await rm(data, { recursive: true, force: true }); }
});
