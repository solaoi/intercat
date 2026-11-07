import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { promisify } from 'node:util';
import path from 'node:path';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

test.describe('explicit request corrections', () => {
  let app: ElectronApplication;
  let data: string;
  test.beforeEach(async () => {
    data = await mkdtemp('/private/tmp/ic-feedback-');
    app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const now = Date.now();
      const word = { id: 'old', text: 'この依頼の宛先を取り違えました。'.repeat(60), state: 'landed', revision: 1, createdAt: now - 60000,
        target: { mode: 'none' }, attachments: [], reason: null, correction: null as null | { explicit: number; inferred: number; misread: boolean },
        decision: { kind: 'fork', trackId: 'A', boundary: 'now', command: null, confidence: .95, inferenceMs: 7, timedOut: false } };
      const state = { connected: true, sequence: 1, utterances: [], workers: [],
        tracks: [{ id: 'A', label: 'A', title: 'Original worker', status: 'done' }, { id: 'B', label: 'B', title: 'Next worker', status: 'waiting' }],
        timeline: { clock: { epochMs: now, monoMs: 0, timelineMs: now, bootId: 'fixture' }, summaries: [] } };
      const fixture = { word, state, loseNext: false, failNext: false, calls: [] as { opId: string; utteranceId: string; revision: number }[], receipts: new Set<string>() };
      Object.assign(globalThis, { feedbackFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], removed: [], removedMarks: [], marks: [
        { id: 'input/old', inputId: 'old', trackId: 'A', repairRunId: null, at: now - 60000, text: 'この依頼の宛先を取り違えました。', operation: 'fork' },
      ] }));
      ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', () => structuredClone(word));
      ipcMain.removeHandler('intercat:utterance-misread'); ipcMain.handle('intercat:utterance-misread', (_event, request) => {
        fixture.calls.push(request);
        if (fixture.failNext) { fixture.failNext = false; throw new Error('The correction could not be saved.'); }
        if (!fixture.receipts.has(request.opId)) {
          fixture.receipts.add(request.opId);
          word.correction = { explicit: 1, inferred: 0, misread: true };
          state.sequence++;
        }
        if (fixture.loseNext) { fixture.loseNext = false; throw new Error('The saved correction acknowledgement was lost.'); }
        return structuredClone(word);
      });
    });
    await (await app.firstWindow()).reload();
  });
  test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });

  test('marks an old request from the timeline and preserves the next draft at both zoom levels', async () => {
    const page = await app.firstWindow();
    await page.getByRole('combobox', { name: 'Request destination', exact: true }).selectOption('B');
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Next instruction for B');
    await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
    const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      const button = dialog.getByRole('button', { name: 'Mark as misread', exact: true });
      await expect(button).toBeInViewport();
      expect(await dialog.evaluate(element => { const box = element.getBoundingClientRect(); return box.left >= 0 && box.right <= innerWidth && box.top >= 0 && box.bottom <= innerHeight; })).toBe(true);
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      await page.waitForTimeout(200);
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`misread-${zoom}.png`), Buffer.from(pixels, 'base64'));
    }
    await dialog.getByRole('button', { name: 'Mark as misread', exact: true }).click();
    await expect(dialog).toContainText('Marked as misread.');
    await expect(dialog.getByRole('button', { name: 'Mark as misread', exact: true })).toBeDisabled();
    await expect(dialog).toBeFocused();
    await dialog.getByRole('button', { name: 'Close request details' }).click();
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Next instruction for B');
    await expect(page.getByRole('combobox', { name: 'Request destination', exact: true })).toHaveValue('B');
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(1));
    await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
    await expect(dialog).toContainText('Marked as misread.');
    expect(await app.evaluate(() => (globalThis as typeof globalThis & { feedbackFixture: { calls: unknown[] } }).feedbackFixture.calls.length)).toBe(1);
  });

  for (const failure of ['failNext', 'loseNext'] as const) {
    test(`retry preserves the correction operation after ${failure}`, async () => {
      const page = await app.firstWindow();
      await app.evaluate((_electron, failure) => { (globalThis as typeof globalThis & { feedbackFixture: Record<string, unknown> }).feedbackFixture[failure] = true; }, failure);
      await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
      const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
      await dialog.getByRole('button', { name: 'Mark as misread', exact: true }).click();
      await expect(dialog.getByRole('button', { name: 'Retry correction' })).toBeEnabled();
      await dialog.getByRole('button', { name: 'Retry correction' }).click();
      await expect(dialog).toContainText('Marked as misread.');
      const calls = await app.evaluate(() => (globalThis as typeof globalThis & { feedbackFixture: { calls: unknown[] } }).feedbackFixture.calls);
      expect(calls).toHaveLength(2); expect(calls[1]).toEqual(calls[0]);
    });
  }

  test('a request still being delivered cannot be marked as misread', async () => {
    await app.evaluate(() => { (globalThis as typeof globalThis & { feedbackFixture: { word: { state: string } } }).feedbackFixture.word.state = 'sent'; });
    const page = await app.firstWindow();
    await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
    const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(dialog).toContainText('This request has already been sent.');
    await expect(dialog.getByRole('button', { name: 'Mark as misread', exact: true })).toHaveCount(0);
  });
});

test('native correction survives restarting the application and daemon', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180000);
  const execute = promisify(execFile);
  const data = await mkdtemp('/private/tmp/ic-native-feedback-');
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  const stop = async () => {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* It may already have stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false);
      pid = undefined;
    }
  };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    const word = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'A dummy request marked as misread', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await stop();
    const track = '2052912c-ab98-4228-806a-f07d001f1d48';
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
db=sqlite3.connect(sys.argv[1]); track=sys.argv[2]; word=sys.argv[3]
db.execute('INSERT INTO tracks VALUES(?,?,?,?)',(track,'A','Recorded request','done'))
decision={'kind':'fork','trackId':track,'boundary':'now','command':None,'confidence':1.0,'inferenceMs':1.0,'timedOut':False,'scores':{}}
db.execute("UPDATE utterances SET state='landed',decision=? WHERE id=?",(json.dumps(decision),word)); db.commit()`, path.join(data, 'daemon/events.sqlite'), track, word.id]);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
    const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    await dialog.getByRole('button', { name: 'Mark as misread', exact: true }).click();
    await expect(dialog).toContainText('Marked as misread.');
    const saved = await page.evaluate(id => window.intercat.getUtterance(id), word.id);
    expect(saved.correction).toEqual({ explicit: 1, inferred: 0, misread: true });
    await stop();
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
    await expect(page.getByRole('dialog', { name: 'Request', exact: true })).toContainText('Marked as misread.');
    const restored = await page.evaluate(id => window.intercat.getUtterance(id), word.id);
    expect(restored.correction).toEqual(saved.correction);
    expect(restored.revision).toBe(word.revision);
    await writeFile(test.info().outputPath('native-feedback.json'), JSON.stringify({ saved, restored }, null, 2));
  } finally { await stop(); await rm(data, { recursive: true, force: true }); }
});

test('native inferred correction follows Take back and Put across delayed delivery and restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180000);
  const execute = promisify(execFile);
  const data = await mkdtemp('/private/tmp/ic-native-inference-');
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
  const seed = async (input: string, track: string, label: string, state: string) => {
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
input,track,label,state=sys.argv[2:]
db=sqlite3.connect(sys.argv[1]); db.execute('INSERT INTO tracks VALUES(?,?,?,?)',(track,label,'Recorded request','done'))
decision={'kind':'fork','trackId':track,'boundary':'now','command':None,'confidence':1.0,'inferenceMs':1.0,'timedOut':False,'scores':{}}
db.execute('UPDATE utterances SET state=?,decision=? WHERE id=?',(state,json.dumps(decision),input)); db.commit()`, path.join(data, 'daemon/events.sqlite'), input, track, label, state]);
  };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    const old = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Adjust the recorded heading spacing', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await stop();
    await seed(old.id, '2052912c-ab98-4228-806a-f07d001f1d48', 'A', 'placed');
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await page.evaluate(id => window.intercat.cutUtterance(id), old.id);
    const next = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Adjust the recorded heading spacing', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await stop();
    await seed(next.id, 'a20243e1-9181-447e-bb39-343c75ca26d1', 'B', 'landed');
    for (let restart = 0; restart < 2; restart++) {
      app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
      page = await app.firstWindow();
      await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
      pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
      await expect.poll(async () => (await page.evaluate(id => window.intercat.getUtterance(id), old.id)).correction, { timeout: 10000 }).toEqual({ explicit: 0, inferred: 1, misread: false });
      expect((await page.evaluate(id => window.intercat.getUtterance(id), next.id)).state).toBe('landed');
      if (restart === 0) await stop();
    }
    await writeFile(test.info().outputPath('native-inferred.json'), JSON.stringify({ original: await page.evaluate(id => window.intercat.getUtterance(id), old.id), replacement: await page.evaluate(id => window.intercat.getUtterance(id), next.id) }, null, 2));
  } finally { await stop(); await rm(data, { recursive: true, force: true }); }
});
