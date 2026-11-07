import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

const execute = promisify(execFile);

test('a new track keeps its chosen source turn through native UI and daemon restarts', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(240_000);
  const data = await mkdtemp('/private/tmp/ic-start-condition-');
  const model = path.join(data, 'model');
  const python = path.join(data, 'python');
  await mkdir(model);
  await writeFile(python, '#!/usr/bin/python3\n' + await readFile('tests/fixtures/park_editor.py', 'utf8'), { mode: 0o700 });
  let app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
  let pid: number | undefined;
  const stop = async () => {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already exited. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
      pid = undefined;
    }
  };
  const start = async () => {
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    const page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    return page;
  };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await configureEditor(app, data, model, python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('After this turn, prepare the first source');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    const emptyCard = page.getByRole('group', { name: 'Request needs attention', exact: true });
    await expect(emptyCard).toContainText('No running turn was recorded');
    await expect(emptyCard.getByRole('button', { name: /^After .+’s turn$/ })).toHaveCount(0);
    await emptyCard.getByRole('button', { name: 'When I say', exact: true }).click();
    await expect(emptyCard).toHaveCount(0);
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Second source');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(2);
    const tracks = (await page.evaluate(() => window.intercat.getStatus())).tracks!;
    await stop();
    // Seed the running facts. Admission, source choice, persistence and UI use real RPC.
    const sources = tracks.map((track, index) => ({
      trackId: track.id, label: track.label, sessionId: 'source-' + index,
      turnId: ['00000000-0000-7000-8000-000000000001', '00000000-0000-7000-8000-000000000002'][index],
    }));
    await execute('/usr/bin/python3', ['-c', `import json,sqlite3,sys
db=sqlite3.connect(sys.argv[1])
for source in json.loads(sys.argv[2]):
 worker=dict(trackId=source['trackId'],phase='working',sessionId=source['sessionId'],activeTurn=source['turnId'],identity=None,detail=None,checkpointPending=False,checkpoint=None,approvals=[],prompts=[],output=None)
 db.execute('INSERT OR REPLACE INTO workers VALUES(?,?)',(source['trackId'],json.dumps(worker)))
 turn=dict(track=source['trackId'],repair_run_id=None,current=None,index=0,terminal=False)
 db.execute('INSERT INTO timeline_turns VALUES(?,?,?)',(source['sessionId'],source['turnId'],json.dumps(turn)))
db.commit()`, path.join(data, 'daemon/events.sqlite'), JSON.stringify(sources)]);
    page = await start();
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    const text = 'After this turn, 新しい独立作業を始める';
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill(text);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    const card = page.getByRole('group', { name: 'Request needs attention', exact: true });
    await expect(card).toContainText('Which running turn should the new track wait for?');
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      const choice = card.getByRole('button', { name: 'After B’s turn', exact: true });
      await choice.focus();
      await expect(choice).toBeInViewport();
      await expect(choice).toBeEnabled();
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`source-choice-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
    }
    await card.getByRole('button', { name: 'After B’s turn', exact: true }).click();
    await expect(card).toHaveCount(0);
    await expect(page.locator('.request-chip').filter({ hasText: text })).toContainText('new track · after B’s turn');
    let current = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.text === text)!;
    expect(current.startCondition?.source).toEqual(sources[1]);
    expect(current.startCondition?.ready).toBe(false);
    expect(current.state).toBe('placed');
    const target = current.decision!.trackId;
    await page.locator('.request-select').filter({ hasText: text }).click();
    const details = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(details).toContainText('after B’s recorded turn');
    await expect(details.getByRole('button', { name: 'Close request details', exact: true })).toBeInViewport();
    await details.getByRole('button', { name: 'Close request details', exact: true }).click();
    await stop();
    page = await start();
    current = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.text === text)!;
    expect(current.startCondition?.source).toEqual(sources[1]);
    expect(current.startCondition?.ready).toBe(false);
    expect(current.decision!.trackId).toBe(target);
    await stop();
    await execute('/usr/bin/python3', ['-c', `import sqlite3,sys
db=sqlite3.connect(sys.argv[1])
db.execute("UPDATE timeline_turns SET record=json_set(record,'$.terminal',json('true')) WHERE session=? AND turn=?",sys.argv[2:])
db.commit()`, path.join(data, 'daemon/events.sqlite'), sources[1].sessionId, sources[1].turnId]);
    page = await start();
    current = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(word => word.text === text)!;
    expect(current.startCondition?.ready).toBe(true);
    expect(current.decision!.trackId).toBe(target);
    await writeFile(test.info().outputPath('restored.json'), JSON.stringify(current, null, 2));
  } finally {
    await stop();
    await rm(data, { recursive: true, force: true });
  }
});
