import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, access, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import path from 'node:path';
const execute = promisify(execFile);
const exists = async (file: string) => { try { await access(file); return true; } catch { return false; } };

test('a report question holds queued work until its reply is sent and checkpointed', async () => {
  test.skip(process.env.INTERCAT_TEST_RUN_WORKER !== '1', 'Opt-in paid dummy worker turns');
  test.setTimeout(240000);
  const data = await mkdtemp('/private/tmp/ic-report-ui-'); const source = path.join(data, 'source');
  await mkdir(source); const environment = { PATH: process.env.PATH, HOME: data, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env: environment });
  await git(['init', '-b', 'main']); await writeFile(path.join(source, 'README'), 'Dummy report-question fixture.\n');
  await git(['add', '.']); await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'fixture']);
  const app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] }); let pid: number | undefined;
  try {
    const page = await app.firstWindow(); await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => { const picks = paths.map(file => ({ canceled: false, filePaths: [file] })); dialog.showOpenDialog = async () => picks.shift()!; }, [source, process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await configureEditor(app, data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Start now. Do not use tools and do not change files. Ask me what answer.txt should contain. End the final answer with this exact standalone line: Waiting for you: What should answer.txt contain? Do not choose the contents yourself.');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
    const question = page.getByRole('group', { name: 'Question from Worker A' });
    await expect(question).toContainText('What should answer.txt contain?', { timeout: 120000 });
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpointPending, { timeout: 15000 }).toBe(false);
    const state = await page.evaluate(() => window.intercat.getStatus()); const track = state.tracks![0].id; const qid = state.reportQuestions![0].id; const working = state.workspaces![0].path;
    const normal = await page.evaluate(async trackId => window.intercat.stageUtterance({ attachmentIds: [], opId: await window.intercat.createOperationId(), text: 'Create later.txt containing queued on one line. Do not change other files. Report completion.', target: { mode: 'lock', trackId }, selectedTrackId: trackId }), track);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.id === normal.id)?.state, { timeout: 15000 }).toBe('placed');
    await page.waitForTimeout(500); expect(await exists(path.join(working, 'later.txt'))).toBe(false);
    await question.getByRole('button', { name: 'Reply', exact: true }).click();
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Write answer.txt containing exactly the 8 ASCII characters approved followed by one newline. Nothing else belongs in that file. Do not change any other file and do not ask another question. Report completion.');
    await page.getByRole('button', { name: 'Send reply', exact: true }).click();
    await expect.poll(async () => exists(path.join(working, 'answer.txt')), { timeout: 60000 }).toBe(true);
    expect((await readFile(path.join(working, 'answer.txt'), 'utf8')).trim()).toBe('approved');
    // An uncertain report is deliberately held for review, not implicitly allowed.
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.replyTo?.id === qid)?.state, { timeout: 60000 }).toBe('landed');
    await expect.poll(async () => {
      const current = await page.evaluate(() => window.intercat.getStatus());
      return (current.reportQuestions?.length ?? 0) > 0 || await exists(path.join(working, 'later.txt'));
    }, { timeout: 60000 }).toBe(true);
    const after = await page.evaluate(() => window.intercat.getStatus());
    for (const review of after.reportQuestions ?? []) {
      expect(review.kind).toBe('review');
      await page.evaluate(async q => window.intercat.respondToReport({ opId: await window.intercat.createOperationId(), questionId: q.id, revision: q.revision, answer: { action: 'continue' } }), review);
    }
    await expect.poll(async () => exists(path.join(working, 'later.txt')), { timeout: 60000 }).toBe(true);
    expect((await readFile(path.join(working, 'later.txt'), 'utf8')).trim()).toBe('queued');
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.id === normal.id)?.state, { timeout: 15000 }).toBe('landed');
    const final = await page.evaluate(() => window.intercat.getStatus());
    const reply = final.utterances!.find(word => word.replyTo?.id === qid)!;
    expect(reply.state).toBe('landed');
    const order = await execute('/usr/bin/python3', ['-c', 'import sqlite3,json,sys; c=sqlite3.connect(sys.argv[1]); print(json.dumps([(s,json.loads(p)["id"]) for s,p in c.execute("select seq,payload from events where kind=\'utterance.execution\'") if json.loads(p).get("state")=="sent"]))', path.join(data, 'daemon/events.sqlite')]);
    const sent = JSON.parse(order.stdout) as [number, string][];
    expect(sent.find(([_, id]) => id === reply.id)![0]).toBeLessThan(sent.find(([_, id]) => id === normal.id)![0]);
    expect(await exists(path.join(source, 'answer.txt'))).toBe(false); expect(await exists(path.join(source, 'later.txt'))).toBe(false);
    await mkdir('research/verify/report-ui-20261104', { recursive: true }); await page.screenshot({ path: 'research/verify/report-ui-20261104/completed.png' });
  } finally {
    await mkdir('research/verify/report-ui-20261104', { recursive: true });
    try { await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); d=sqlite3.connect(sys.argv[2]); s.backup(d); d.close(); s.close()', path.join(data, 'daemon/events.sqlite'), path.resolve('research/verify/report-ui-20261104/events.sqlite')]); } catch { /* Keep the original setup failure. */ }
    await closeWindow(app); if (pid) { process.kill(pid, 'SIGTERM'); await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15000 }).toBe(false); }
    await rm(data, { recursive: true, force: true });
  }
});
