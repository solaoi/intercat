import { mockDrafts } from './support/drafts';
import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
const execute = promisify(execFile);

test('a closed step receives a summary through the native daemon and keeps its address', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180_000);
  const data = await mkdtemp('/private/tmp/ic-clip-summary-');
  const model = path.join(data, 'model');
  const python = path.join(data, 'python');
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
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Add a zero-case test');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(1);
    const track = (await page.evaluate(() => window.intercat.getStatus())).tracks![0].id;
    await closeWindow(app);
    process.kill(pid!, 'SIGTERM');
    await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    pid = undefined;

    // Seed one persisted MSP step while the isolated daemon is stopped.
    const now = Date.now();
    const stamp = (offset: number) => ({ epochMs: now + offset, monoMs: offset, timelineMs: now + offset, bootId: 'fixture' });
    const clip = { id: 'f121150b-0c3a-5e2d-9aa4-f71cc5b80e1c', trackId: track, repairRunId: null, ordinal: 1, sessionId: 'fixture-session', turnId: 'fixture-turn', start: stamp(-120_000), end: stamp(-40_000), generationEnd: stamp(-40_000), category: 'step', shellItem: null, recovered: false };
    const item = { itemId: 'answer', turnId: 'fixture-turn', kind: 'agentMessage', revision: 1, status: 'completed', text: 'Added a zero-case test. No tests were run.' };
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
db=sqlite3.connect(sys.argv[1]); clip=json.loads(sys.argv[2]); item=json.loads(sys.argv[3])
db.execute('INSERT INTO timeline_clips VALUES(?,?,?,?)',(clip['id'],clip['trackId'],clip['ordinal'],json.dumps(clip)))
db.execute('INSERT INTO timeline_items VALUES(?,?,?,?,?,?)',(clip['sessionId'],item['itemId'],clip['id'],1,json.dumps(item),json.dumps(item)))
db.commit(); db.close()`, path.join(data, 'daemon/events.sqlite'), JSON.stringify(clip), JSON.stringify(item)]);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    const button = page.getByRole('button', { name: 'Open step A:1: Added a zero-case test without running the suite.', exact: true });
    await expect(button).toBeAttached({ timeout: 30_000 });
    await button.focus(); await button.press('Enter');
    const summary = page.getByRole('region', { name: 'Step summary' });
    await expect(summary).toContainText('Zero-case test');
    await expect(summary).toContainText('Added a zero-case test without running the suite.');
    const saved = await page.evaluate(id => window.intercat.getClipDetail(id), clip.id);
    expect(saved.clip.id).toBe(clip.id);
    expect(saved.clip.trackId).toBe(track);
    expect(saved.clip.ordinal).toBe(1);
    expect(saved.items[0].text).toBe(item.text);
    await writeFile(test.info().outputPath('native-summary.json'), JSON.stringify(saved, null, 2));
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('native-summary.png'), Buffer.from(pixels, 'base64'));
  } finally {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have exited. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    await rm(data, { recursive: true, force: true });
  }
});

test('a late summary refreshes an open step and long Japanese text wraps at 200 percent', async () => {
  const data = await mkdtemp('/private/tmp/ic-summary-preview-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  try {
    const page = await app.firstWindow();
    await app.evaluate(({ ipcMain }) => {
      const now = Date.now();
      const clip = { id: 'summary-preview', trackId: 'A', ordinal: 14, sessionId: 's', turnId: 't', category: 'step', label: 'Reply', kind: 'think', start: now - 130_000, end: now - 45_000, running: false, generationEnd: null, approximate: false, toolTicks: [] };
      const state = { connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: '試験の追加', status: 'done' }], utterances: [], workers: [], timeline: { summaries: [], clock: { epochMs: now, monoMs: 0, bootId: 'fixture', timelineMs: now } } };
      Object.assign(globalThis, { summaryFixture: { state, clip } });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [clip], removed: [], marks: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:clip-detail'); ipcMain.handle('intercat:clip-detail', () => ({ clip, items: [] }));
    });
    const open = page.getByRole('button', { name: 'Open step A:14: Reply', exact: true });
    await expect(open).toBeAttached(); await open.focus(); await open.press('Enter');
    await expect(page.getByRole('region', { name: 'Step summary' })).toHaveCount(0);
    const title = '境界値の試験を追加';
    const text = 'ゼロと負の値の試験を追加しましたが、試験はまだ実行していません。'.repeat(18);
    await app.evaluate((_electron, value) => {
      const fixture = (globalThis as typeof globalThis & { summaryFixture: { state: { sequence: number }; clip: { title?: string; summary?: string; label: string } } }).summaryFixture;
      Object.assign(fixture.clip, value, { label: value.summary }); fixture.state.sequence++;
    }, { title, summary: text });
    const summary = page.getByRole('region', { name: 'Step summary' });
    await expect(summary).toContainText(text);
    await app.evaluate(({ BrowserWindow }) => {
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2);
    });
    await summary.scrollIntoViewIfNeeded();
    const horizontal = await summary.evaluate(element => ({ width: element.clientWidth, content: element.scrollWidth }));
    expect(horizontal.content).toBeLessThanOrEqual(horizontal.width);
    await expect(page.getByRole('heading', { name: title, exact: true })).toBeInViewport();
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('summary-japanese-200.png'), Buffer.from(pixels, 'base64'));
  } finally {
    await closeWindow(app); await rm(data, { recursive: true, force: true });
  }
});
