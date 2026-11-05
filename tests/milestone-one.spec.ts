import { closeWindow } from './support/close';
import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { timelineClips, timelineMarks } from './support/timeline';

const execute = promisify(execFile);

test('M1 follows one login track from request through denied and allowed effects to Commit', async () => {
  test.skip(process.env.INTERCAT_M1_ACCEPTANCE !== '1', 'Opt-in paid dummy repository acceptance');
  test.setTimeout(600000);
  for (const name of ['INTERCAT_TEST_EDITOR_MODEL', 'INTERCAT_TEST_EDITOR_PYTHON', 'INTERCAT_TEST_MUSE_BINARY']) expect(process.env[name], name).toBeTruthy();
  const data = await mkdtemp('/private/tmp/ic-m1-acceptance-');
  const output = path.resolve(process.env.INTERCAT_M1_OUTPUT!);
  await mkdir(output, { recursive: true });
  const source = path.join(data, 'source');
  const remote = path.join(data, 'remote.git');
  await mkdir(source);
  const env = { PATH: process.env.PATH, HOME: data, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  const git = async (...args: string[]) => (await execute('/usr/bin/git', ['-C', source, ...args], { env })).stdout.trim();
  await git('init', '-b', 'main');
  await writeFile(path.join(source, 'package.json'), JSON.stringify({ name: 'm1-login-fixture', version: '1.0.0', private: true, scripts: { test: 'node --test login.test.cjs' } }));
  await writeFile(path.join(source, 'package-lock.json'), JSON.stringify({ name: 'm1-login-fixture', version: '1.0.0', lockfileVersion: 3, requires: true, packages: { '': { name: 'm1-login-fixture', version: '1.0.0' } } }));
  await writeFile(path.join(source, 'login.cjs'), 'async function login(attempt) { return attempt(); }\nexports.login = login;\n');
  await writeFile(path.join(source, 'login.test.cjs'), `const test = require('node:test');
const assert = require('node:assert/strict');
const { login } = require('./login.cjs');
test('login retries temporary failures and returns the successful result', async () => {
  await new Promise(resolve => setTimeout(resolve, 8000));
  let attempts = 0;
  assert.equal(await login(async () => { if (++attempts < 3) throw new Error('temporary'); return 'signed-in'; }), 'signed-in');
  assert.equal(attempts, 3);
});
test('login stops after three failed attempts', async () => {
  let attempts = 0;
  await assert.rejects(login(async () => { attempts++; throw new Error('temporary'); }), /temporary/);
  assert.equal(attempts, 3);
});
`);
  await git('add', '.');
  await git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'Login acceptance fixture');
  await execute('/usr/bin/git', ['init', '--bare', remote], { env });
  await git('remote', 'add', 'origin', remote);
  const base = await git('rev-parse', 'HEAD');
  const server = createServer((_request, response) => response.end('fixture'));
  let connections = 0;
  server.on('connection', () => { connections++; });
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  if (!address || typeof address === 'string') throw new Error('Listener not ready');
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  let pid: number | undefined;
  const observations: Record<string, unknown> = { data, source, remote, base, connections: 0, passed: false };
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, choices) => {
      const queue = choices.map(file => ({ canceled: false, filePaths: [file] }));
      dialog.showOpenDialog = async () => queue.shift()!;
    }, [source, process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await configureEditor(app, data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
    const first = 'ログイン関数にリトライを足して、テストを走らせて。最後に git push origin HEAD:refs/heads/try-a を走らせて';
    await page.getByRole('textbox', { name: 'Request draft' }).fill(first);
    await page.evaluate(text => {
      const timings = { click: 0, painted: 0 };
      Object.assign(globalThis, { m1ChipTiming: timings });
      document.addEventListener('click', () => { timings.click = performance.now(); }, { capture: true, once: true });
      const observer = new MutationObserver(() => {
        if ([...document.querySelectorAll('.request-chip')].some(node => node.textContent?.includes(text))) {
          observer.disconnect();
          requestAnimationFrame(() => { timings.painted = performance.now(); });
        }
      });
      observer.observe(document.body, { subtree: true, childList: true, characterData: true });
    }, first);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => page.evaluate(() => (globalThis as unknown as { m1ChipTiming: { painted: number } }).m1ChipTiming.painted)).toBeGreaterThan(0);
    const timing = await page.evaluate(() => (globalThis as unknown as { m1ChipTiming: { click: number; painted: number } }).m1ChipTiming);
    observations.chipMs = timing.painted - timing.click;
    await writeFile(path.join(output, 'observations.json'), JSON.stringify(observations, null, 2));
    expect(observations.chipMs).toBeLessThanOrEqual(100);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.activeTurn, { timeout: 60000 }).toBeTruthy();
    const queueText = 'CHANGELOG にも一行足して';
    await page.getByRole('textbox', { name: 'Request draft' }).fill(queueText);
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    const queued = page.getByRole('button', { name: `Cut ${queueText} from A`, exact: true });
    await expect(queued).toBeVisible({ timeout: 15000 });
    await expect(queued).toHaveAttribute('title', /after this turn/);
    await queued.focus();
    await page.evaluate(() => {
      const events: unknown[] = [];
      Object.assign(globalThis, { m1Keys: events });
      document.addEventListener('keydown', event => events.push({ key: event.key, target: (event.target as HTMLElement).outerHTML.slice(0, 300) }), true);
    });
    await queued.press('Backspace');
    observations.keys = await page.evaluate(() => (globalThis as unknown as { m1Keys: unknown[] }).m1Keys);
    await expect(queued).toHaveCount(0);
    const approval = page.getByRole('group', { name: 'Approval for Worker A' });
    await expect(approval).toContainText('push', { timeout: 180000 });
    expect((await execute('/usr/bin/git', ['ls-remote', remote], { env })).stdout.trim()).toBe('');
    await page.screenshot({ path: path.join(output, 'denied-push.png') });
    await approval.getByRole('button', { name: 'Don’t run it', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.sha, { timeout: 90000 }).toBeTruthy();
    const firstDone = await page.evaluate(() => window.intercat.getStatus());
    observations.firstDone = firstDone;
    const worker = firstDone.workers![0];
    expect(firstDone.tracks).toHaveLength(1);
    expect(firstDone.utterances?.find(word => word.text === queueText)?.state).toBe('cut');
    expect((await execute('/usr/bin/git', ['ls-remote', remote], { env })).stdout.trim()).toBe('');
    expect((await timelineMarks(page)).some(mark => mark.text === first)).toBe(true);
    expect((await timelineClips(page)).some(clip => clip.kind === 'edit')).toBe(true);
    await execute('npm', ['test'], { cwd: firstDone.workspaces![0].path, env });
    await expect(page.getByRole('log', { name: 'Worker A result' })).toContainText(worker.checkpoint!.sha);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).reports?.length ?? 0, { timeout: 10000 }).toBe(0);
    expect((await page.evaluate(() => window.intercat.getStatus())).reportQuestions).toHaveLength(0);
    await page.getByRole('textbox', { name: 'Request draft' }).fill('もう一度 git push origin HEAD:refs/heads/try-a を走らせて');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect(approval).toContainText('push', { timeout: 90000 });
    expect((await execute('/usr/bin/git', ['ls-remote', remote], { env })).stdout.trim()).toBe('');
    await approval.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.turnId, { timeout: 90000 }).not.toBe(worker.checkpoint!.turnId);
    const pushed = await page.evaluate(() => window.intercat.getStatus());
    observations.pushed = pushed;
    expect(pushed.workers![0].sessionId).toBe(worker.sessionId);
    expect(pushed.tracks).toHaveLength(1);
    expect(pushed.effects?.some(effect => effect.kind === 'push' && effect.outcome === 'done')).toBe(true);
    expect((await execute('/usr/bin/git', ['ls-remote', remote, 'refs/heads/try-a'], { env })).stdout).toContain('refs/heads/try-a');
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByLabel('Stops', { exact: true }).selectOption('every-approval');
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByRole('dialog', { name: 'Settings · App' })).not.toBeVisible();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).stops).toBe('every-approval');
    await page.getByRole('textbox', { name: 'Request draft' }).fill(`今すぐ bash で curl http://127.0.0.1:${address.port}/ を一度だけ実行して結果を報告して。他のコマンドや再試行はしないで。`);
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect(approval).toContainText('curl', { timeout: 90000 });
    expect(connections).toBe(0);
    await approval.getByRole('button', { name: 'Allow once', exact: true }).click();
    await expect(approval).toContainText('"kind":"network"', { timeout: 30000 });
    await expect(approval).toContainText(`127.0.0.1:${address.port}`);
    expect(connections).toBe(0);
    await approval.getByRole('button', { name: 'Don’t run it', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.checkpoint?.turnId, { timeout: 90000 }).not.toBe(pushed.workers![0].checkpoint!.turnId);
    expect(connections).toBe(0);
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByLabel('Stops', { exact: true }).selectOption('irreversible-only');
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByRole('dialog', { name: 'Settings · App' })).not.toBeVisible();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).stops).toBe('irreversible-only');
    await page.getByRole('button', { name: 'Commit', exact: true }).click();
    const sheet = page.getByRole('complementary', { name: 'Commit review' });
    await expect(sheet).toContainText('Checked and ready', { timeout: 60000 });
    await expect(sheet).toContainText('Effects outside');
    observations.review = (await page.evaluate(() => window.intercat.getStatus())).integration;
    await page.screenshot({ path: path.join(output, 'review.png') });
    await sheet.getByRole('button', { name: 'Commit', exact: true }).click();
    await expect(sheet).toContainText('Committed', { timeout: 30000 });
    const committed = await page.evaluate(() => window.intercat.getStatus());
    observations.committed = committed;
    const refs = (await git('for-each-ref', '--format=%(refname)', 'refs/heads/intercat/')).split('\n');
    expect(refs).toHaveLength(1);
    observations.commitMessage = await git('show', '-s', '--format=%B', refs[0]);
    expect(observations.commitMessage).toContain('Intercat:');
    expect(await git('rev-parse', 'HEAD')).toBe(base);
    await page.screenshot({ path: path.join(output, 'committed.png') });
    const savedClips = await timelineClips(page);
    const savedMarks = await timelineMarks(page);
    await closeWindow(app);
    process.kill(pid!, 'SIGTERM');
    await expect.poll(() => { try { process.kill(pid!, 0); return false; } catch { return true; } }).toBe(true);
    pid = undefined;
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    const restored = await page.evaluate(() => window.intercat.getStatus());
    pid = restored.daemonPid;
    expect(restored.workers?.every(worker => worker.activeTurn === null && worker.phase === 'stopped')).toBe(true);
    for (const worker of restored.workers ?? []) {
      if (worker.identity) expect(() => process.kill(worker.identity!.pid, 0)).toThrow();
    }
    expect(await timelineClips(page)).toEqual(savedClips);
    expect(await timelineMarks(page)).toEqual(savedMarks);
    expect(restored.integration?.id).toBe(committed.integration?.id);
    expect(restored.integration?.status).toBe('done');
    observations.restored = restored;
    observations.passed = true;
  } finally {
    observations.connections = connections;
    try { observations.final = await (await app.firstWindow()).evaluate(() => window.intercat.getStatus()); } catch { /* The application may already be closed. */ }
    await writeFile(path.join(output, 'observations.json'), JSON.stringify(observations, null, 2));
    try { await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); d=sqlite3.connect(sys.argv[2]); s.backup(d)', path.join(data, 'daemon/events.sqlite'), path.join(output, 'events.sqlite')]); } catch { /* Preserve setup errors. */ }
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return false; } catch { return true; } }, { timeout: 15000 }).toBe(true);
    }
    await new Promise<void>(resolve => server.close(() => resolve()));
  }
});
