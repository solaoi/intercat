import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { promisify } from 'node:util';
import { execFile } from 'node:child_process';
import path from 'node:path';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

test.describe('confirmation mode', () => {
  let app: ElectronApplication;
  let directory: string;
  test.beforeEach(async () => {
    directory = await mkdtemp('/private/tmp/ic-confirmation-');
    app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const mode = { policy: 'automatic', active: true, revision: 1, suggested: true, suggestedFor: 'pending', notice: 'I’ll ask before placing requests.', rates: { requests: 10, explicit: 3, inferred: 1 } };
      const word = { id: 'pending', text: '別の作業を新しいトラックで進めてください。'.repeat(30), state: 'asked', revision: 1, target: { mode: 'none' }, createdAt: 1, attachments: [],
        decision: { kind: 'fork', trackId: null, boundary: 'now', command: null, confidence: .95, inferenceMs: 1, timedOut: false }, confirmation: { decisionId: 'original-decision', turn: null }, reason: 'waiting for OK' };
      const fixture = { mode, word, fail: false, replaceOnConfirm: false, calls: [] as unknown[], receipts: new Set<string>() };
      Object.assign(globalThis, { confirmationFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => ({ connected: true, sequence: mode.revision + word.revision, confirmation: mode, utterances: [word], tracks: [], workers: [] }));
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', () => structuredClone(word));
      ipcMain.removeHandler('intercat:utterance-confirm'); ipcMain.handle('intercat:utterance-confirm', (_event, request) => {
        fixture.calls.push(request);
        if (fixture.replaceOnConfirm) {
          fixture.replaceOnConfirm = false; word.revision++; word.confirmation.decisionId = 'replacement-decision';
          throw new Error('The interpretation changed.');
        }
        if (!fixture.receipts.has(request.opId)) {
          if (request.revision !== word.revision || request.decisionId !== word.confirmation?.decisionId) throw new Error('The interpretation changed.');
          fixture.receipts.add(request.opId); word.state = 'placed'; word.revision++; Object.assign(word, { confirmation: null });
        }
        if (fixture.fail) { fixture.fail = false; throw new Error('The acknowledgement was lost.'); }
        return structuredClone(word);
      });
      ipcMain.removeHandler('intercat:confirmation-change'); ipcMain.handle('intercat:confirmation-change', (_event, request) => {
        fixture.calls.push(request);
        mode.revision++; mode.active = request.action.kind !== 'back-to-automatic' && request.action.policy === 'always';
        mode.policy = request.action.kind === 'back-to-automatic' ? 'automatic' : request.action.policy;
        mode.notice = mode.active ? 'I’ll ask before placing requests.' : 'Back to automatic.';
        return structuredClone(mode);
      });
    });
    await (await app.firstWindow()).reload();
  });
  test.afterEach(async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); });

  test('waiting interpretations never place themselves and Enter on the chip confirms them', async () => {
    const page = await app.firstWindow();
    const chip = page.getByRole('group', { name: 'Interpretation waiting for OK' });
    await expect(chip).toContainText('New track · now');
    await page.waitForTimeout(1400);
    expect(await app.evaluate(() => (globalThis as typeof globalThis & { confirmationFixture: { calls: unknown[] } }).confirmationFixture.calls)).toHaveLength(0);
    await chip.focus(); await page.keyboard.press('Enter');
    await expect(chip).toHaveCount(0);
  });

  test('Back to automatic leaves the displayed proposal waiting for its own OK', async () => {
    const page = await app.firstWindow();
    const chip = page.getByRole('group', { name: 'Interpretation waiting for OK' });
    await chip.getByText('Change ▾', { exact: true }).click();
    await chip.getByRole('button', { name: 'Back to automatic', exact: true }).click();
    await expect(chip).toContainText('waiting for OK');
    await expect(chip.getByText('back to automatic?', { exact: true })).toHaveCount(0);
    await chip.getByRole('button', { name: 'OK', exact: true }).click();
    await expect(chip).toHaveCount(0);
  });

  test('a replaced interpretation rejects the old OK and presents the new identity', async () => {
    const page = await app.firstWindow();
    const chip = page.getByRole('group', { name: 'Interpretation waiting for OK' });
    await expect(chip).toBeVisible();
    await app.evaluate(() => {
      (globalThis as typeof globalThis & { confirmationFixture: { replaceOnConfirm: boolean } }).confirmationFixture.replaceOnConfirm = true;
    });
    await chip.getByRole('button', { name: 'OK', exact: true }).click();
    await expect(chip.getByRole('button', { name: 'OK', exact: true })).toBeEnabled();
    await chip.getByRole('button', { name: 'OK', exact: true }).click();
    await expect(chip).toHaveCount(0);
  });

  test('long Japanese proposals and the settings exit remain usable at 200 percent', async () => {
    const page = await app.firstWindow();
    await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
    const chip = page.getByRole('group', { name: 'Interpretation waiting for OK' });
    await chip.getByRole('button', { name: 'OK', exact: true }).focus();
    await expect(chip.getByRole('button', { name: 'OK', exact: true })).toBeInViewport();
    const collapsed = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('confirmation-collapsed-200.png'), Buffer.from(collapsed, 'base64'));
    await chip.getByText('Change ▾', { exact: true }).click();
    await chip.getByRole('button', { name: 'Back to automatic' }).focus();
    await expect(chip.getByRole('button', { name: 'Back to automatic' })).toBeInViewport();
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('confirmation-200.png'), Buffer.from(pixels, 'base64'));
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(1));
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    const settings = page.getByRole('dialog', { name: 'Settings · App' });
    await expect(settings.getByLabel('Editor · Confirmation mode', { exact: true })).toBeDisabled();
    await settings.getByRole('button', { name: 'Back to automatic', exact: true }).click();
    await expect(settings.getByLabel('Editor · Confirmation mode', { exact: true })).toBeEnabled();
    await settings.getByLabel('Editor · Confirmation mode', { exact: true }).selectOption('off');
    await expect(settings.getByLabel('Editor · Confirmation mode', { exact: true })).toHaveValue('off');
  });

  test('a focused OK stays visible after zoom beside completed workers and a proposal', async () => {
    await app.evaluate(({ ipcMain }) => {
      const fixture = (globalThis as typeof globalThis & { confirmationFixture: { mode: unknown; word: unknown } }).confirmationFixture;
      const tracks = ['A', 'B'].map(id => ({ id, label: id, title: 'Write a dummy file', status: 'done' }));
      const workers = tracks.map(track => ({ trackId: track.id, phase: 'stopped', sessionId: `dummy-${track.id}`, activeTurn: null,
        identity: null, detail: 'Turn completed', checkpointPending: false, checkpoint: null, approvals: [], prompts: [],
        output: 'Created the requested dummy file.\nNo tests were requested.\n'.repeat(5) }));
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, workerConfigured: true,
        session: { id: 'dummy', name: 'Write a dummy file', slug: 'dummy', createdAt: 1 },
        tracks, workers, confirmation: fixture.mode, utterances: [fixture.word],
        proposal: { id: 'dummy-proposal', kind: 'work-completed', trackId: 'A', label: 'A', createdAt: Date.now(),
          text: 'A is done — commit the dummy file?', detail: 'Review the saved dummy changes.',
          options: [{ action: 'commit', text: 'Review for commit' }, { action: 'later', text: 'Not now' }] },
      }));
    });
    const page = await app.firstWindow();
    await page.reload();
    const chip = page.getByRole('group', { name: 'Interpretation waiting for OK', exact: true });
    await chip.getByText('Change ▾', { exact: true }).click();
    await chip.getByRole('button', { name: 'Back to automatic', exact: true }).click();
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      const accept = chip.getByRole('button', { name: 'OK', exact: true });
      await accept.focus();
      await expect(accept).toBeInViewport({ ratio: 1 });
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`focused-confirmation-${zoom}.png`), Buffer.from(pixels, 'base64'));
    }
    await chip.getByRole('button', { name: 'OK', exact: true }).click();
    await expect(chip).toHaveCount(0);
  });
});

test('native confirmation enters on explicit corrections, survives restart and keeps shown OKs after exit', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180000);
  const execute = promisify(execFile);
  const directory = await mkdtemp('/private/tmp/ic-native-confirmation-');
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
  let pid: number | undefined;
  async function stop() {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have exited. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false);
      pid = undefined;
    }
  }
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    const ids = await page.evaluate(async () => {
      const ids: string[] = [];
      for (let index = 0; index < 10; index++) ids.push((await window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: `Recorded request ${index}`, target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] })).id);
      return ids;
    });
    await stop();
    await execute('/usr/bin/python3', ['-c', `import sqlite3,json,sys,uuid
db=sqlite3.connect(sys.argv[1]);track=str(uuid.uuid4());db.create_function('intercat_mono',0,lambda:0);db.create_function('intercat_boot',0,lambda:'fixture');db.create_function('intercat_position',0,lambda:0)
db.execute('INSERT INTO tracks VALUES(?,?,?,?)',(track,'A','Recorded work','done'))
decision={'kind':'extend','trackId':track,'boundary':'now','command':None,'confidence':.95,'inferenceMs':1,'timedOut':False,'scores':{}}
db.execute('UPDATE utterances SET state=?,decision=?',('landed',json.dumps(decision)));db.commit()`, path.join(directory, 'daemon/events.sqlite')]);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    for (const id of ids.slice(0, 4)) await page.evaluate(async id => window.intercat.markMisread({ opId: await window.intercat.createOperationId(), utteranceId: id, revision: 1 }), id);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).confirmation?.active).toBe(true);
    const pending = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: '確認してから新しい作業を開始', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await stop();
    await execute('/usr/bin/python3', ['-c', `import sqlite3,json,sys,uuid
db=sqlite3.connect(sys.argv[1]);db.create_function('intercat_mono',0,lambda:0);db.create_function('intercat_boot',0,lambda:'fixture');db.create_function('intercat_position',0,lambda:0)
meaning={'kind':'fork','trackId':None,'boundary':'now','command':None};decision=dict(meaning,confidence=.95,inferenceMs=1,timedOut=False,scores={})
db.execute('UPDATE utterances SET state=?,decision=?,reason=? WHERE id=?',('asked',json.dumps(decision),'waiting for OK',sys.argv[2]))
db.execute('INSERT INTO input_confirmations VALUES(?,?,?,?,NULL)',(sys.argv[2],str(uuid.uuid4()),json.dumps(meaning,sort_keys=True,separators=(',',':')),None));db.commit()`, path.join(directory, 'daemon/events.sqlite'), pending.id]);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    const chip = page.getByRole('group', { name: 'Interpretation waiting for OK' });
    await expect(chip).toBeVisible();
    await chip.getByText('Change ▾', { exact: true }).click();
    await chip.getByRole('button', { name: 'Back to automatic', exact: true }).click();
    await expect(chip).toBeVisible();
    await chip.getByRole('button', { name: 'OK', exact: true }).click();
    await expect(chip).toHaveCount(0);
    const status = await page.evaluate(() => window.intercat.getStatus());
    expect(status.confirmation?.active).toBe(false);
    expect(status.tracks).toHaveLength(2);
    await writeFile(test.info().outputPath('native-confirmation.json'), JSON.stringify({ confirmation: status.confirmation, word: await page.evaluate(id => window.intercat.getUtterance(id), pending.id) }, null, 2));
  } finally { await stop(); await rm(directory, { recursive: true, force: true }); }
});
