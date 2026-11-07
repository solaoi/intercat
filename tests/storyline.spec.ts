import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { promisify } from 'node:util';
import path from 'node:path';
import type { TimelineMark } from '../src/bridge';
import { groupMarks, markLabel } from '../src/timeline/inputMarkLayout';
import { initialView } from '../src/timeline/model';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

const execute = promisify(execFile);
const tracks = [{ id: 'A', label: 'A' }, { id: 'B', label: 'B' }];

test('statement labels describe every result without exposing its operation name', () => {
  const mark: TimelineMark = { id: 'word', inputId: 'word', trackId: 'A', repairRunId: null, at: 0, text: 'A request' };
  for (const [operation, result] of [['extend', 'to A'], ['fork', 'new track'], ['trim', 'redirect A'], ['park', 'parked'], ['ask', 'asked'], ['plan', 'planned'], ['schedule', 'after A'], ['command', 'commit']] as const) {
    expect(markLabel({ ...mark, operation, command: 'commit' }, tracks)).toBe(result);
  }
  expect(markLabel({ ...mark, operation: 'park', trackId: null }, tracks)).toBe('parked');
  expect(markLabel({ ...mark, repairRunId: 'repair' }, tracks)).toBe('to A · for the commit');
});

test('dense statement groups preserve every member in time order and reserve 24 pixels', () => {
  const marks = Array.from({ length: 600 }, (_, index) => ({ id: String(index), inputId: String(index), trackId: null, repairRunId: null, at: (initialView.start + 10 + index * 0.1) * 1000, text: 'Request ' + index }));
  const groups = groupMarks([...marks].reverse(), initialView, 1040);
  expect(groups.flatMap(group => group.marks)).toEqual(marks);
  expect(groups.length).toBeLessThan(10);
  for (let index = 1; index < groups.length; index++) expect(groups[index].x - groups[index - 1].x).toBeGreaterThanOrEqual(24);
});

test.describe('statement details', () => {
  let app: ElectronApplication;
  let data: string;
  test.beforeEach(async () => {
    data = await mkdtemp('/private/tmp/ic-storyline-');
    app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const now = Date.now();
      const state = { connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: 'Earlier work', status: 'done' }, { id: 'B', label: 'B', title: 'Other work', status: 'waiting' }], workers: [], utterances: [],
        timeline: { clock: { epochMs: now, monoMs: 0, timelineMs: now, bootId: 'fixture' }, summaries: [] } };
      const marks = [
        { id: 'old', inputId: 'old', at: now - 270000, text: 'Earlier request 日本語', operation: 'fork', trackId: 'A', repairRunId: null },
        { id: 'near', inputId: 'near', at: now - 269000, text: 'An idea for later', operation: 'park', trackId: null, repairRunId: null },
        { id: 'ask', inputId: 'ask', at: now - 80000, text: 'Which destination?', operation: 'ask', trackId: null, repairRunId: null },
      ];
      const requests: string[] = [];
      Object.assign(globalThis, { storylineFixture: { state, marks, requests } });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks, removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', (_event, id) => {
        requests.push(id);
        return { id, text: id === 'old' ? 'Earlier request 日本語. This is the complete preserved request.' : 'An idea for later', state: 'landed', createdAt: now - 270000, revision: 1, target: { mode: 'none' }, reason: null, attachments: [],
          decision: { kind: id === 'old' ? 'fork' : 'park', trackId: id === 'old' ? 'A' : null, boundary: 'now', command: null, confidence: .95, inferenceMs: 7, timedOut: false } };
      });
    });
  });
  test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });

  test('dense marks open old request details and preserve the draft destination at 200 percent', async () => {
    const page = await app.firstWindow();
    await page.getByRole('combobox', { name: 'Request destination', exact: true }).selectOption('B');
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Still addressed to B');
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(1); });
      const group = page.getByRole('button', { name: '2 requests at this time', exact: true });
      await group.scrollIntoViewIfNeeded();
      const bounds = (await group.boundingBox())!;
      expect(bounds.width).toBeGreaterThanOrEqual(24); expect(bounds.height).toBeGreaterThanOrEqual(24);
      await group.focus(); await group.press('Enter');
      const choices = page.getByRole('dialog', { name: 'Requests at this time', exact: true });
      if (zoom === 2) await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2));
      await expect(choices).toBeInViewport();
      expect(await choices.evaluate(element => { const box = element.getBoundingClientRect(); return box.left >= 0 && box.right <= innerWidth && box.top >= 0 && box.bottom <= innerHeight; })).toBe(true);
      await expect(choices).toContainText('new track');
      await expect(choices).toContainText('parked');
      await choices.getByRole('button', { name: /Earlier request/ }).click();
      const details = page.getByRole('dialog', { name: 'Request', exact: true });
      await expect(details).toContainText('This is the complete preserved request.');
      await details.getByText('Decision details', { exact: true }).click();
      await expect(details.locator('.decision-details')).toContainText('fork');
      await expect(page.locator('#track-A')).toHaveAttribute('aria-selected', 'true');
      await expect(page.getByRole('combobox', { name: 'Request destination', exact: true })).toHaveValue('B');
      await expect(details.getByRole('button', { name: 'Close request details' })).toBeInViewport();
      await expect(details).toContainText('This request has already been sent.');
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`details-${zoom}.png`), Buffer.from(pixels, 'base64'));
      await details.getByRole('button', { name: 'Close request details' }).click();
    }
    const requests = await app.evaluate(() => (globalThis as typeof globalThis & { storylineFixture: { requests: string[] } }).storylineFixture.requests);
    expect(requests).toEqual(['old', 'old']);
  });

  test('the Home storyline is read without a track and live decisions replace their marks', async () => {
    const page = await app.firstWindow();
    await app.evaluate(() => {
      const fixture = (globalThis as typeof globalThis & { storylineFixture: { state: { tracks: unknown[]; sequence: number }; marks: TimelineMark[] } }).storylineFixture;
      fixture.state.tracks = [];
      fixture.marks = [fixture.marks[2]];
      fixture.state.sequence++;
    });
    await app.evaluate(({ ipcMain }) => {
      const fixture = (globalThis as typeof globalThis & { storylineFixture: { state: { sequence: number }; marks: TimelineMark[] } }).storylineFixture;
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', (_event, query) => {
        if (query.tracks.length) throw new Error('Home must not need a track');
        return { revision: fixture.state.sequence, reset: true, clips: [], marks: fixture.marks, removed: [], removedMarks: [] };
      });
    });
    const inputs = page.getByLabel('Timeline requests', { exact: true });
    await expect(inputs.getByRole('button')).toHaveCount(1);
    await expect(inputs.getByRole('button', { name: 'Which destination? · asked', exact: true })).toBeVisible();
    await app.evaluate(() => {
      const fixture = (globalThis as typeof globalThis & { storylineFixture: { state: { sequence: number }; marks: TimelineMark[] } }).storylineFixture;
      fixture.marks[0] = { ...fixture.marks[0], operation: 'plan' }; fixture.state.sequence++;
    });
    await expect(inputs.getByRole('button', { name: 'Which destination? · planned', exact: true })).toBeVisible();
  });

  test('outcome labels remain on the timeline outside the request popup', async () => {
    const page = await app.firstWindow();
    await app.evaluate(({ BrowserWindow }) => {
      const fixture = (globalThis as typeof globalThis & { storylineFixture: { state: { sequence: number }; marks: TimelineMark[] } }).storylineFixture;
      const now = Date.now();
      const values: [TimelineMark['operation'], string, string | null][] = [['fork', 'Add tests', 'A'], ['trim', 'Change course', 'B'], ['park', 'A later idea', null], ['ask', 'Which worker?', null]];
      fixture.marks.splice(0, fixture.marks.length, ...values.map(([operation, text, trackId], index) => ({ id: `label-${index}`, inputId: `label-${index}`, operation, text, trackId, repairRunId: null, at: now - 360000 + index * 100000 })));
      fixture.state.sequence++;
      BrowserWindow.getAllWindows()[0].setSize(1440, 900);
    });
    await expect(page.getByLabel('Timeline requests').getByRole('button')).toHaveCount(4);
    await expect(page.getByRole('button', { name: 'Change course · redirect B', exact: true })).toBeVisible();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await page.waitForTimeout(250);
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('timeline-outcomes.png'), Buffer.from(pixels, 'base64'));
  });

  test('a delayed statement read does not reopen after the user selects another track', async () => {
    const page = await app.firstWindow();
    await app.evaluate(({ ipcMain }) => {
      ipcMain.removeHandler('intercat:utterance-read');
      ipcMain.handle('intercat:utterance-read', () => new Promise(resolve => {
        Object.assign(globalThis, { finishStatement: () => resolve({ id: 'old', text: 'Late detail', state: 'landed', createdAt: 1, revision: 1,
          target: { mode: 'none' }, reason: null, attachments: [], decision: null }) });
      }));
    });
    await page.getByRole('button', { name: '2 requests at this time', exact: true }).click();
    await page.getByRole('dialog', { name: 'Requests at this time', exact: true }).getByRole('button', { name: /Earlier request/ }).click();
    await expect.poll(() => app.evaluate(() => 'finishStatement' in globalThis)).toBe(true);
    await page.getByRole('gridcell', { name: /^B · Other work/ }).click();
    await app.evaluate(() => (globalThis as typeof globalThis & { finishStatement: () => void }).finishStatement());
    await page.waitForTimeout(100);
    await expect(page.getByRole('dialog', { name: 'Request', exact: true })).toHaveCount(0);
    await expect(page.locator('#track-B')).toHaveAttribute('aria-selected', 'true');
  });
});

test('native statements retain their original time and open complete stored details after restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180000);
  const data = await mkdtemp('/private/tmp/ic-native-storyline-');
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
    const word = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Review this recorded dummy request. '.repeat(10), target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
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
    const marks = await page.evaluate(async () => { const now = Date.now(); return window.intercat.getTimelineWindow({ tracks: [], start: now - 300000, end: now + 1000, since: null }); });
    expect(marks.marks).toHaveLength(1);
    expect(marks.marks[0].operation).toBe('fork');
    expect(marks.marks[0].text.endsWith('…')).toBe(true);
    expect(marks.marks[0].text.length).toBeLessThan(word.text.length);
    expect(Math.abs(marks.marks[0].at - word.createdAt)).toBeLessThan(2000);
    await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
    const details = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(details.locator('.request-original')).toHaveText(word.text);
    await details.getByText('Decision details', { exact: true }).click();
    await expect(details.locator('.decision-details')).toHaveAttribute('open', '');
    await expect(details.locator('.decision-details')).toContainText(track);
    await writeFile(test.info().outputPath('native-mark.json'), JSON.stringify({ word, marks }, null, 2));
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    await page.waitForTimeout(200);
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('native-details.png'), Buffer.from(pixels, 'base64'));
  } finally { await stop(); await rm(data, { recursive: true, force: true }); }
});
