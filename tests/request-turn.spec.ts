import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';

test.describe('request turn details', () => {
let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-request-turn-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }) => {
    const now = Date.now();
    const identity = { utteranceId: 'old', sessionId: 'original-session', turnId: 'original-turn' };
    const origin = { identity, trackId: 'A', label: 'A', phase: 'running', checkpoint: null as string | null };
    const fixture = { origin, rejectStop: false, delayFile: false, calls: [] as unknown[], finishFile: null as null | (() => void) };
    Object.assign(globalThis, { turnFixture: fixture });
    const state = { connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: 'Original work', status: 'working' }], workers: [], utterances: [],
      timeline: { clock: { epochMs: now, monoMs: 0, timelineMs: now, bootId: 'fixture' }, summaries: [] } };
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], removed: [], removedMarks: [], marks: [{ id: 'input/old', inputId: 'old', trackId: 'A', repairRunId: null, at: now - 60000, text: 'Original request', operation: 'fork' }] }));
    ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', () => ({ id: 'old', text: 'Original request', state: 'landed', revision: 1, createdAt: now - 60000, target: { mode: 'none' }, attachments: [], reason: null,
      decision: { kind: 'fork', trackId: 'A', boundary: 'now', command: null, confidence: 1, inferenceMs: 1, timedOut: false } }));
    ipcMain.removeHandler('intercat:utterance-origin'); ipcMain.handle('intercat:utterance-origin', () => structuredClone(origin));
    ipcMain.removeHandler('intercat:utterance-stop'); ipcMain.handle('intercat:utterance-stop', (_event, request) => {
      fixture.calls.push(request);
      if (fixture.rejectStop) { origin.phase = 'moved-on'; origin.checkpoint = 'original-checkpoint'; throw new Error('The worker has moved on from this request’s turn.'); }
      origin.phase = 'stopping';
    });
    ipcMain.removeHandler('intercat:utterance-changes'); ipcMain.handle('intercat:utterance-changes', (_event, request) => {
      fixture.calls.push({ kind: 'changes', identity: request });
      return { identity, label: 'A', checkpoint: 'original-checkpoint', files: ['a.txt', 'nested/日本語.txt'] };
    });
    ipcMain.removeHandler('intercat:utterance-diff'); ipcMain.handle('intercat:utterance-diff', (_event, request) => {
      fixture.calls.push({ kind: 'diff', ...request });
      const diff = { path: request.path, before: 'before this turn', after: request.path === 'a.txt' ? 'original turn only' : '日本語の変更だけ', redacted: false, binary: false, tooLarge: false };
      if (fixture.delayFile && request.path === 'a.txt') return new Promise(resolve => { fixture.finishFile = () => resolve(diff); });
      return diff;
    });
  });
  await (await app.firstWindow()).reload();
});
test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });

async function openRequest() {
  const page = await app.firstWindow();
  await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
  return page.getByRole('dialog', { name: 'Request', exact: true });
}

test('the stop control sends the identity of the request’s original turn', async () => {
  const popup = await openRequest();
  await popup.getByRole('button', { name: 'Stop A', exact: true }).click();
  await expect(popup.getByLabel('Turn started by this request')).toContainText('Stopping A…');
  await expect(popup.getByRole('button', { name: 'Stop A', exact: true })).toHaveCount(0);
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { turnFixture: { calls: unknown[] } }).turnFixture.calls);
  expect(calls).toEqual([expect.objectContaining({ identity: { utteranceId: 'old', sessionId: 'original-session', turnId: 'original-turn' } })]);
});

test('a worker that moves on while the click is in flight loses its Stop control', async () => {
  await app.evaluate(() => { (globalThis as typeof globalThis & { turnFixture: { rejectStop: boolean } }).turnFixture.rejectStop = true; });
  const popup = await openRequest();
  await popup.getByRole('button', { name: 'Stop A', exact: true }).click();
  await expect(popup).toContainText('The worker has moved on');
  await expect(popup.getByRole('button', { name: 'Stop A', exact: true })).toHaveCount(0);
  await expect(popup.getByRole('button', { name: 'Show A’s changes' })).toBeVisible();
});

test('saved changes stay tied to the old checkpoint and fit at 200 percent', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { turnFixture: { origin: { phase: string; checkpoint: string } } }).turnFixture; fixture.origin.phase = 'moved-on'; fixture.origin.checkpoint = 'original-checkpoint'; });
  const popup = await openRequest();
  await expect(popup).toContainText('A has moved on');
  await popup.getByRole('button', { name: 'Show A’s changes' }).click();
  const changes = page.getByRole('dialog', { name: 'A’s changes', exact: true });
  await expect(changes.locator('.monaco-diff-editor')).toContainText('original turn only');
  for (const zoom of [1, 2]) {
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
    await expect(changes.getByRole('button', { name: 'Close turn changes' })).toBeInViewport();
    await changes.getByRole('button', { name: 'nested/日本語.txt', exact: true }).click();
    await expect(changes.locator('.monaco-diff-editor')).toContainText('日本語の変更だけ');
    expect(await changes.evaluate(element => { const rect = element.getBoundingClientRect(); return rect.left >= 0 && rect.right <= innerWidth && rect.top >= 0 && rect.bottom <= innerHeight; })).toBe(true);
    await page.waitForTimeout(250);
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`turn-changes-${zoom}.png`), Buffer.from(pixels, 'base64'));
  }
  await changes.getByRole('button', { name: 'Close turn changes' }).click();
  await expect(popup).toBeVisible();
  await expect(popup.getByRole('button', { name: 'Show A’s changes' })).toBeFocused();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { turnFixture: { calls: { kind: string; checkpoint?: string }[] } }).turnFixture.calls);
  expect(calls.filter(call => call.kind === 'diff').every(call => call.checkpoint === 'original-checkpoint')).toBe(true);
});

test('the completed turn waits for its saved checkpoint before offering changes', async () => {
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { turnFixture: { origin: { phase: string; checkpoint: string | null } } }).turnFixture; fixture.origin.phase = 'saving'; fixture.origin.checkpoint = null; });
  const popup = await openRequest();
  await expect(popup).toContainText('Saving A’s changes');
  await expect(popup.getByRole('button', { name: 'Stop A', exact: true })).toHaveCount(0);
  await expect(popup.getByRole('button', { name: 'Show A’s changes' })).toHaveCount(0);
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { turnFixture: { origin: { phase: string; checkpoint: string } } }).turnFixture; fixture.origin.phase = 'finished'; fixture.origin.checkpoint = 'original-checkpoint'; });
  await expect(popup.getByRole('button', { name: 'Show A’s changes' })).toBeVisible();
});

test('a late file response cannot replace a different selected file', async () => {
  await app.evaluate(() => { const fixture = (globalThis as typeof globalThis & { turnFixture: { delayFile: boolean; origin: { phase: string; checkpoint: string } } }).turnFixture; fixture.delayFile = true; fixture.origin.phase = 'finished'; fixture.origin.checkpoint = 'original-checkpoint'; });
  const popup = await openRequest();
  await popup.getByRole('button', { name: 'Show A’s changes' }).click();
  const changes = (await app.firstWindow()).getByRole('dialog', { name: 'A’s changes', exact: true });
  await expect.poll(() => app.evaluate(() => !!(globalThis as typeof globalThis & { turnFixture: { finishFile: unknown } }).turnFixture.finishFile)).toBe(true);
  await changes.getByRole('button', { name: 'nested/日本語.txt', exact: true }).click();
  await expect(changes.locator('.monaco-diff-editor')).toContainText('日本語の変更だけ');
  await app.evaluate(() => (globalThis as typeof globalThis & { turnFixture: { finishFile: () => void } }).turnFixture.finishFile());
  await expect(changes.locator('.monaco-diff-editor')).not.toContainText('original turn only');
});

});

test('native request details keep the original checkpoint after a later completed turn and restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180000);
  const execute = promisify(execFile);
  const directory = await mkdtemp('/private/tmp/ic-native-origin-');
  let native = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
  let pid: number | undefined;
  const stop = async () => {
    await closeWindow(native);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have exited. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false);
      pid = undefined;
    }
  };
  try {
    let page = await native.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    const word = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Original change request', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await stop();
    const seeded = await execute('/usr/bin/python3', ['-c', String.raw`import json,sqlite3,sys,uuid,subprocess,os,time
root=sys.argv[1]; word=sys.argv[2]; repo=os.path.join(root,'original-repo'); track='2052912c-ab98-4228-806a-f07d001f1d48'; session='original-session'
env=dict(os.environ,GIT_CONFIG_NOSYSTEM='1',GIT_CONFIG_GLOBAL='/dev/null',GIT_AUTHOR_NAME='Fixture',GIT_AUTHOR_EMAIL='fixture@example.test',GIT_COMMITTER_NAME='Fixture',GIT_COMMITTER_EMAIL='fixture@example.test')
def git(*args,data=None): return subprocess.run(['git','--git-dir='+repo,*args],input=data,text=True,env=env,capture_output=True,check=True).stdout.strip()
subprocess.run(['git','init','--bare',repo],env=env,capture_output=True,check=True)
def tree(text):
 blob=git('hash-object','-w','--stdin',data=text); return git('mktree',data='100644 blob '+blob+'\tfile.txt\n')
base=git('commit-tree',tree('before the request\n'),'-m','base')
original=git('commit-tree',tree('original request edit\n'),'-p',base,'-m','original turn')
later=git('commit-tree',tree('new unrelated turn\n'),'-p',original,'-m','later turn')
reference='refs/intercat/origin/tracks/A'; git('update-ref',reference,later)
command=str(uuid.uuid4()); turn=str(uuid.uuid4()); newer=str(uuid.uuid4()); checkpoint={'turnId':turn,'sha':original,'reference':reference+'-checkpoints/'+turn,'unmergedPaths':[],'liveProcesses':False}
db=sqlite3.connect(os.path.join(root,'daemon','events.sqlite'))
db.create_function('intercat_mono',0,lambda:int(time.monotonic()*1000)); db.create_function('intercat_boot',0,lambda:'fixture'); db.create_function('intercat_position',0,lambda:int(time.time()*1000))
db.execute('INSERT INTO tracks VALUES(?,?,?,?)',(track,'A','Original work','done'))
decision={'kind':'fork','trackId':track,'boundary':'now','command':None,'confidence':1.0,'inferenceMs':1.0,'timedOut':False,'scores':{}}
db.execute('UPDATE utterances SET state=?,decision=?,dispatch_command=? WHERE id=?',('landed',json.dumps(decision),command,word))
db.execute('INSERT INTO operations VALUES(?,?,?,?)',(command,'turn.start',json.dumps({'sessionId':session,'commandId':command}),'complete'))
db.execute('INSERT INTO attachment_turns VALUES(?,?,?,?,?)',(session,command,turn,1,0))
db.execute('INSERT INTO timeline_turns VALUES(?,?,?)',(session,turn,json.dumps({'track':track,'repair_run_id':None,'current':None,'index':0,'terminal':True})))
db.execute('INSERT INTO workspaces VALUES(?,?)',(track,json.dumps({'trackId':track,'path':repo,'reference':reference,'base':base,'state':'ready','error':None,'deniedRoots':[],'attachmentRoots':[]})))
db.execute('INSERT INTO operations VALUES(?,?,?,?)',(str(uuid.uuid5(uuid.UUID(turn),'checkpoint')),'checkpoint.save',json.dumps({'trackId':track,'checkpoint':checkpoint}),'complete'))
worker={'trackId':track,'phase':'ready','sessionId':session,'secretRevision':None,'identity':None,'detail':None,'activeTurn':None,'approvals':[],'output':None,'checkpointPending':False,'checkpoint':checkpoint,'prompts':[]}
db.execute('INSERT INTO workers VALUES(?,?)',(track,json.dumps(worker))); db.commit()
print(json.dumps({'original':original,'later':later,'newer':newer,'track':track,'session':session}))`, directory, word.id]);
    const fixture = JSON.parse(seeded.stdout) as { original: string; later: string; newer: string; track: string; session: string };
    const results = [];
    for (const moved of [false, true]) {
      native = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
      page = await native.firstWindow();
      await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
      pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
      await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
      const popup = page.getByRole('dialog', { name: 'Request', exact: true });
      if (moved) await expect(popup).toContainText('A has moved on');
      await popup.getByRole('button', { name: 'Show A’s changes' }).click();
      const changes = page.getByRole('dialog', { name: 'A’s changes', exact: true });
      await expect(changes.locator('.monaco-diff-editor')).toContainText('original request edit');
      await expect(changes.locator('.monaco-diff-editor')).not.toContainText('new unrelated turn');
      const origin = await page.evaluate(id => window.intercat.getRequestOrigin(id), word.id);
      expect(origin?.checkpoint).toBe(fixture.original);
      results.push(origin);
      await changes.getByRole('button', { name: 'Close turn changes' }).click();
      await popup.getByRole('button', { name: 'Close request details' }).click();
      await stop();
      if (!moved) await execute('/usr/bin/python3', ['-c', String.raw`import json,sqlite3,sys,uuid
db=sqlite3.connect(sys.argv[1]); row=db.execute('SELECT record FROM workers WHERE track_id=?',(sys.argv[2],)).fetchone(); worker=json.loads(row[0]); worker['checkpoint']['turnId']=sys.argv[3]; worker['checkpoint']['sha']=sys.argv[4]; db.execute('UPDATE workers SET record=? WHERE track_id=?',(json.dumps(worker),sys.argv[2])); db.execute('INSERT INTO attachment_turns VALUES(?,?,?,?,?)',(sys.argv[5],str(uuid.uuid4()),sys.argv[3],2,0)); db.commit()`, path.join(directory, 'daemon/events.sqlite'), fixture.track, fixture.newer, fixture.later, fixture.session]);
    }
    await writeFile(test.info().outputPath('native-original-turn.json'), JSON.stringify({ fixture, results }, null, 2));
  } finally { await stop(); await rm(directory, { recursive: true, force: true }); }
});
