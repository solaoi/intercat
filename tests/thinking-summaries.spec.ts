import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { mockDrafts } from './support/drafts';
const execute = promisify(execFile);

test('view observations follow the selected lane without changing a locked draft', async () => {
  const data = await mkdtemp('/private/tmp/ic-thinking-view-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  try {
    const page = await app.firstWindow();
    await page.clock.install();
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const calls: unknown[] = [];
      Object.assign(globalThis, { observations: calls });
      ipcMain.removeHandler('intercat:timeline-observe'); ipcMain.handle('intercat:timeline-observe', (_event, view) => { calls.push(view); });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1,
        tracks: [{ id: 'A', label: 'A', title: 'First task', status: 'working' }, { id: 'B', label: 'B', title: 'Second task', status: 'working' }],
        workers: [], utterances: [] }));
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    });
    await page.clock.runFor(1100);
    const calls = () => app.evaluate(() => (globalThis as typeof globalThis & { observations: unknown[] }).observations);
    await expect.poll(calls).toContainEqual({ trackId: 'A', repairRunId: null });
    await page.getByRole('combobox', { name: 'Request destination' }).selectOption('A');
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Keep this draft on A');
    await page.getByRole('gridcell', { name: 'B · Second task · waiting', exact: true }).click();
    await expect.poll(calls).toContainEqual({ trackId: 'B', repairRunId: null });
    await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('A');
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Keep this draft on A');
    const before = (await calls()).length;
    await page.evaluate(() => { Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); });
    await page.clock.runFor(5100);
    await expect.poll(async () => (await calls()).length).toBeGreaterThan(before);
    expect((await calls()).at(-1)).toEqual({ trackId: 'B', repairRunId: null });
    await page.clock.resume();
  } finally {
    await closeWindow(app); await rm(data, { recursive: true, force: true });
  }
});

test('only the selected thinking lane is refreshed in the native daemon', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180_000);
  const data = await mkdtemp('/private/tmp/ic-thinking-native-');
  const model = path.join(data, 'model'); const python = path.join(data, 'python');
  await mkdir(model);
  await writeFile(python, '#!/usr/bin/python3\n' + await readFile('tests/fixtures/park_editor.py', 'utf8'), { mode: 0o700 });
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await configureEditor(app, data, model, python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    for (const text of ['First thinking lane', 'Second thinking lane']) {
      await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill(text);
      await page.getByRole('button', { name: 'New track', exact: true }).click();
    }
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(2);
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('');
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.filter(word => word.decision?.description).length, { timeout: 30_000 }).toBe(2);
    const tracks = (await page.evaluate(() => window.intercat.getStatus())).tracks!;
    await closeWindow(app); process.kill(pid!, 'SIGTERM');
    await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    pid = undefined;
    const now = Date.now();
    const stamp = { epochMs: now - 30_000, monoMs: 1, timelineMs: now - 30_000, bootId: 'fixture' };
    const ids = ['b1a97230-940e-5e1e-811e-e81be035679f', '53c015ff-c8c3-5d10-aae5-307692d0b780'];
    const clips = tracks.map((track, index) => ({ id: ids[index], trackId: track.id, repairRunId: null, ordinal: 1, sessionId: 'thinking-' + index, turnId: 'turn', start: stamp, end: null, generationEnd: null, category: 'step', shellItem: null, recovered: false }));
    const database = path.join(data, 'daemon/events.sqlite');
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
c=sqlite3.connect(sys.argv[1])
for index,clip in enumerate(json.loads(sys.argv[2])):
 item={'itemId':'reasoning','turnId':'turn','kind':'reasoning','revision':1,'status':'inProgress','text':f'Examining lane {index} retry counter.'}
 c.execute('INSERT INTO timeline_clips VALUES(?,?,?,?)',(clip['id'],clip['trackId'],1,json.dumps(clip)))
 c.execute('INSERT INTO timeline_items VALUES(?,?,?,?,?,?)',(clip['sessionId'],'reasoning',clip['id'],1,json.dumps(item),json.dumps(item)))
c.commit()`, database, JSON.stringify(clips)]);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] }); page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    await app.evaluate((_electron, modulePath) => {
      const load = process.getBuiltinModule('module').createRequire(modulePath);
      const { RpcClient } = load(modulePath);
      const original = RpcClient.prototype.request;
      const calls: unknown[] = [];
      Object.assign(globalThis, { timelineObservations: calls });
      RpcClient.prototype.request = function (this: unknown, method: string, params: unknown) {
        if (method !== 'timeline.observe') return original.call(this, method, params);
        const record = { params, result: null as unknown, error: '' };
        calls.push(record);
        return original.call(this, method, params).then((result: unknown) => { record.result = result; return result; }, (error: Error) => { record.error = error.message; throw error; });
      };
    }, path.resolve('electron/rpc-client.mjs'));
    const rowA = page.getByRole('row').filter({ has: page.getByRole('gridcell', { name: /^A ·/ }) });
    await rowA.getByRole('gridcell').click();
    await expect(rowA).toHaveAttribute('aria-selected', 'true');
    const detail = (id: string) => page.evaluate(id => window.intercat.getClipDetail(id), id);
    await expect.poll(async () => (await detail(ids[0])).clip.title, { timeout: 30_000 }).toBe('Examining retry counter');
    expect((await detail(ids[1])).clip.summary).toBeUndefined();
    const rowB = page.getByRole('row').filter({ has: page.getByRole('gridcell', { name: /^B ·/ }) });
    await rowB.getByRole('gridcell').click();
    await expect(rowB).toHaveAttribute('aria-selected', 'true');
    await expect.poll(async () => (await detail(ids[1])).clip.title, { timeout: 30_000 }).toBe('Examining retry counter');
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
c=sqlite3.connect(sys.argv[1])
for session,record in c.execute("SELECT session,record FROM timeline_items WHERE item='reasoning'").fetchall():
 item=json.loads(record); item['text']+=' Checking updated callers.'; item['revision']=2
 c.execute('UPDATE timeline_items SET record=?,revision=2 WHERE session=? AND item=?',(json.dumps(item),session,'reasoning'))
c.commit()`, database]);
    const generations = async () => (await readFile(path.join(data, 'daemon/model-generations.jsonl'), 'utf8')).trim().split('\n').map(line => JSON.parse(line) as { text: string; phase: string }).filter(call => call.phase === 'thinking');
    await expect.poll(async () => (await generations()).filter(call => call.text.includes('lane 1') && call.text.includes('Checking updated callers.')).length, { timeout: 20_000 }).toBeGreaterThan(0);
    expect((await generations()).filter(call => call.text.includes('lane 0')).length).toBe(1);
    expect((await detail(ids[0])).clip.title).toBe('Examining retry counter');
    await writeFile(test.info().outputPath('thinking-generations.json'), JSON.stringify(await generations(), null, 2));
    await page.getByRole('button', { name: /Open step B:1:/ }).focus(); await page.keyboard.press('Enter');
    await expect(page.getByRole('region', { name: 'Step summary' })).toContainText('Examining the retry path');
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('thinking-native.png'), Buffer.from(pixels, 'base64'));
  } finally {
    try {
      const page = await app.firstWindow();
      await writeFile(test.info().outputPath('observation-calls.json'), JSON.stringify(await app.evaluate(() => (globalThis as typeof globalThis & { timelineObservations?: unknown[] }).timelineObservations), null, 2));
      await writeFile(test.info().outputPath('view-state.json'), JSON.stringify(await page.evaluate(() => ({ visibility: document.visibilityState, focused: document.hasFocus(), selected: document.querySelector('[aria-selected="true"]')?.textContent })), null, 2));
      await writeFile(test.info().outputPath('final-status.json'), JSON.stringify(await page.evaluate(() => window.intercat.getStatus()), null, 2));
      await writeFile(test.info().outputPath('generations.jsonl'), await readFile(path.join(data, 'daemon/model-generations.jsonl')));
      await page.screenshot({ path: test.info().outputPath('final-screen.png') });
    } catch { /* Early setup failures may not have a live page or model log. */ }
    try { await writeFile(test.info().outputPath('editor.log'), await readFile(path.join(data, 'daemon/editor.log'))); } catch { /* Setup may not have produced a log. */ }
    await closeWindow(app);
    if (pid) { try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false); }
    await rm(data, { recursive: true, force: true });
  }
});
