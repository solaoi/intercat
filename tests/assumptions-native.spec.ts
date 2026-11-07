import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

const execute = promisify(execFile);

test('native assumptions survive restart and send a changed choice to their own track', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180_000);
  const data = await mkdtemp('/private/tmp/ic-assumption-native-');
  const model = path.join(data, 'model');
  const python = path.join(data, 'python');
  await mkdir(model);
  await writeFile(python, '#!/usr/bin/python3\n' + await readFile('tests/fixtures/assumption_editor.py', 'utf8'), { mode: 0o700 });
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  const stopDaemon = async () => {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have stopped with its window. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
      pid = undefined;
    }
  };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await configureEditor(app, data, model, python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    for (const text of ['Keep the first cache waiting for my signal.', 'Keep the second cache waiting for my signal.']) {
      await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill(text);
      await page.getByRole('button', { name: 'New track', exact: true }).click();
    }
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(2);
    const tracks = (await page.evaluate(() => window.intercat.getStatus())).tracks!;
    await stopDaemon();
    // Seed two recorded worker messages. Extraction, storage, RPC and UI use the normal daemon path.
    const now = Date.now();
    const clips = tracks.map((track, index) => ({
      id: ['bfb6ecf1-9d29-5a14-b3ef-84b6bbf97512', '8dbbca58-18c0-52c8-9f91-8f46b4f69a00'][index],
      trackId: track.id, repairRunId: null, ordinal: 1, sessionId: 'assumption-worker-' + index, turnId: '427bbff3-0671-4cde-9205-91b601c4949b',
      start: { epochMs: now, monoMs: 1, timelineMs: now, bootId: 'fixture' },
      end: null, generationEnd: null, category: 'step', shellItem: null, recovered: false,
    }));
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
connection=sqlite3.connect(sys.argv[1])
for index,clip in enumerate(json.loads(sys.argv[2])):
 item={'itemId':'message','turnId':clip['turnId'],'kind':'agentMessage','revision':1,'status':'inProgress','text':('Routine worker progress.\\n'*2000+'Which database should this cache use, SQLite or PostgreSQL? ' if index else '')+'I assumed '+('SQLite' if index==0 else 'PostgreSQL')+' for this cache and continued.'}
 connection.execute('INSERT INTO timeline_clips VALUES(?,?,?,?)',(clip['id'],clip['trackId'],1,json.dumps(clip)))
 connection.execute('INSERT INTO timeline_items VALUES(?,?,?,?,?,?)',(clip['sessionId'],'message',clip['id'],1,json.dumps(item),json.dumps(item)))
connection.commit()`, path.join(data, 'daemon/events.sqlite'), JSON.stringify(clips)]);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).assumptions?.length, { timeout: 40_000 }).toBe(2);
    const initial = (await page.evaluate(() => window.intercat.getStatus())).assumptions!;
    for(const track of tracks) {
      const log=await page.evaluate(trackId=>window.intercat.getEditorLog({trackId,before:null}),track.id);
      expect(log.entries.filter(entry=>entry.scene==='assumed')).toHaveLength(1);
    }

    expect(initial.find(assumption => assumption.clipId === clips[0].id)?.source).toBe('default-placement');
    expect(initial.every(assumption => Number.isFinite(assumption.observedAt?.timelineMs))).toBe(true);
    const origin = initial.find(assumption => assumption.clipId === clips[1].id)!;
    expect(origin.questionId).toEqual(expect.any(String));
    const originDetail = await page.evaluate(id => window.intercat.getClipDetail(id), clips[1].id);
    expect(origin.source).toBe(originDetail.clip.running ? 'default-placement' : 'question-timeout');
    await expect(page.getByRole('button', { name: /^Open assumptions for / })).toHaveCount(2);
    await writeFile(test.info().outputPath('extracted.json'), JSON.stringify(initial, null, 2));
    const generations = (await readFile(path.join(data, 'assumption-generations.jsonl'), 'utf8')).trim().split('\n').map(line => JSON.parse(line));
    expect(generations.length).toBeGreaterThanOrEqual(3);
    await writeFile(test.info().outputPath('budget-generations.json'), JSON.stringify(generations, null, 2));
    await page.getByRole('gridcell', { name: /^A ·/ }).click();
    const card = page.locator('.assumption-card');
    await expect(card).toHaveCount(1);
    await expect(card).toContainText('SQLite');
    await expect(card).not.toHaveAttribute('open');
    if (!await card.isVisible()) await page.getByRole('button', { name: /^\+\d+ more/ }).click();
    await card.locator('summary').click();
    await card.getByRole('button', { name: 'Keep', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).assumptions?.length).toBe(1);
    const detail = await page.evaluate(id => window.intercat.getClipDetail(id), clips[0].id);
    expect(detail.assumptions?.[0].state).toBe('kept');
    expect(detail.assumptions?.[0].observedAt).toEqual(initial.find(value => value.clipId === clips[0].id)!.observedAt);
    expect(detail.clip.assumptionTimes).toEqual([detail.assumptions![0].observedAt!.timelineMs]);
    await stopDaemon();
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).assumptions?.length).toBe(1);
    await page.getByRole('gridcell', { name: /^B ·/ }).click();
    await expect(page.locator('.assumption-card')).toContainText('PostgreSQL');
    await expect(page.locator('.assumption-card')).toContainText('Worker asked · Which database should this cache use, SQLite or PostgreSQL?');
    await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
    if (!await page.locator('.assumption-card').isVisible()) await page.getByRole('button', { name: /^\+\d+ more/ }).click();
    await page.locator('.assumption-card summary').click();
    const originalQuestion = page.locator('.assumption-card > p').first();
    await originalQuestion.evaluate(element => element.scrollIntoView({ block: 'center' }));
    await expect(originalQuestion).toBeInViewport({ ratio: 1 });
    const questionPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('native-question-200.png'), Buffer.from(questionPixels, 'base64'));
    await page.getByRole('button', { name: 'Change', exact: true }).click();
    await page.getByRole('textbox', { name: 'Change assumption', exact: true }).fill('Use SQLite for this cache.');
    const send = page.getByRole('button', { name: 'Send correction', exact: true });
    await send.scrollIntoViewIfNeeded();
    await expect(send).toBeInViewport();
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeInViewport();
    await page.waitForTimeout(300);
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('native-200.png'), Buffer.from(pixels, 'base64'));
    await send.click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).assumptions?.length).toBe(0);
    const state = await page.evaluate(() => window.intercat.getStatus());
    const correction = state.utterances?.find(word => word.text.includes('Use SQLite for this cache.'));
    expect(correction?.target).toEqual({ mode: 'lock', trackId: tracks[1].id });
    const changedLog=await page.evaluate(trackId=>window.intercat.getEditorLog({trackId,before:null}),tracks[1].id);
    expect(changedLog.entries.filter(entry=>entry.scene==='assumption-change')).toHaveLength(1);

    await writeFile(test.info().outputPath('changed.json'), JSON.stringify(correction, null, 2));
  } finally {
    try { await writeFile(test.info().outputPath('final-status.json'), JSON.stringify(await (await app.firstWindow()).evaluate(() => window.intercat.getStatus()), null, 2)); } catch { /* The app may already be closed. */ }
    for(const file of ['assumption-requests.jsonl','assumption-counts.jsonl']) {
      try { await writeFile(test.info().outputPath(file), await readFile(path.join(data,file))); } catch { /* No request may have reached the fixture. */ }
    }
    try { await writeFile(test.info().outputPath('editor.log'), await readFile(path.join(data, 'daemon/editor.log'))); } catch { /* Early setup may not produce a log. */ }
    await stopDaemon();
    await rm(data, { recursive: true, force: true });
  }
});
