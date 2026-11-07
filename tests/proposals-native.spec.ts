import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';

const execute = promisify(execFile);

test('real worker results reach proposals, continuation and the retained Editor log', async () => {
  test.skip(process.env.INTERCAT_M2_PROPOSAL_NATIVE !== '1', 'Opt-in: three paid disposable Muse turns and existing Keychain access');
  test.setTimeout(600_000);
  const binary = process.env.INTERCAT_TEST_MUSE_BINARY!;
  const prior = Number(process.env.INTERCAT_PREVIOUS_PAID_TURNS);
  expect(binary).toBeTruthy(); expect(Number.isInteger(prior) && prior >= 0).toBe(true);
  const data = await mkdtemp('/private/tmp/ic-proposal-native-');
  const source = path.join(data, 'source'), model = path.join(data, 'model'), python = path.join(data, 'python');
  const output = path.resolve(process.env.INTERCAT_M2_PROPOSAL_OUTPUT!);
  await mkdir(source); await mkdir(model); await mkdir(output, { recursive: true });
  await writeFile(python, '#!/usr/bin/python3\n' + await readFile('tests/fixtures/proposal_editor.py', 'utf8'), { mode: 0o700 });
  const env = { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' };
  const git = (args: string[]) => execute('/usr/bin/git', ['-c', 'core.hooksPath=/dev/null', '-c', 'user.name=Dummy', '-c', 'user.email=dummy@example.invalid', ...args], { cwd: source, env });
  await git(['init', '-q', '--template=', '-b', 'main']);
  await writeFile(path.join(source, 'README.md'), '# Disposable proposal fixture\n');
  await git(['add', 'README.md']); await git(['commit', '-qm', 'Dummy base']);
  let app: ElectronApplication | undefined, page: Page | undefined, daemon: number | undefined, attempted = 0;
  let passed = false;
  let primaryFailure = false;
  const observations: Record<string, unknown> = {};
  const saveBudget = () => writeFile(path.join(output, 'paid-turns.json'), JSON.stringify({ attemptedPaidTurns: attempted, prior, cumulative: prior + attempted }, null, 2));
  const status = () => page!.evaluate(() => window.intercat.getStatus());
  const stopApp = async () => {
    if (app) { await closeWindow(app); app = undefined; }
    if (daemon) {
      try { process.kill(daemon, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(daemon!, 0); return true; } catch { return false; } }, { timeout: 30_000 }).toBe(false);
      daemon = undefined;
    }
  };
  const launch = async () => {
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] }); page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 100_000 });
    daemon = (await status()).daemonPid;
  };
  const snapshot = async (name: string) => {
    const state = await status(); observations[name] = state;
    await writeFile(path.join(output, `${name}.json`), JSON.stringify(state, null, 2)); return state;
  };
  const waitResult = async (track: string, kind: string) => {
    await expect.poll(async () => {
      const state = await status();
      const worker = state.workers?.find(worker => worker.trackId === track);
      if (worker?.approvals.length || worker?.prompts.length || state.reportQuestions?.some(question => question.trackId === track)
        || ['failed', 'lost', 'needs-recovery'].includes(worker?.phase ?? '')) {
        await snapshot('unexpected-worker-state');
        throw new Error(`Dummy worker needs attention: ${worker?.phase}`);
      }
      return state.tracks?.find(item => item.id === track)?.completion?.result?.kind;
    }, { timeout: 150_000, intervals: [250, 500, 1000] }).toBe(kind);
    await expect.poll(async () => {
      const proposal = (await status()).proposal;
      return proposal?.trackId === track ? proposal.options[0].action : null;
    }, { timeout: 30_000 }).toBe(kind === 'partial' ? 'continue' : 'commit');
  };
  const add = async (text: string) => {
    const started = Date.now();
    const before = (await status()).tracks?.map(track => track.id) ?? [];
    await page!.getByRole('textbox', { name: 'Request draft', exact: true }).fill(text);
    attempted++; await saveBudget();
    await page!.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await status()).tracks?.length, { timeout: 90_000 }).toBe(before.length + 1);
    observations[`request${attempted}PlacementMs`] = Date.now() - started;
    return (await status()).tracks!.find(track => !before.includes(track.id))!.id;
  };
  try {
    await saveBudget(); await launch();
    await configureEditor(app!, data, model, python);
    await expect(page!.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    await app!.evaluate(({ dialog }, files) => {
      const choices = [...files]; dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [choices.shift()!] });
    }, [source, binary]);
    expect((await page!.evaluate(() => window.intercat.configureProject())).project).toBeTruthy();
    const configured = await page!.evaluate(() => window.intercat.configureWorkers());
    observations.configureResponse = configured;
    if (!configured.configured && configured.error?.includes('operation outcome may be unknown')) {
      // Configuration is durable and continues after a lost/timed-out reply.
      // Observe that same request; do not spawn a second verifier/host.
      await expect.poll(async () => (await status()).workerConfigured, { timeout: 120_000 }).toBe(true);
    } else {
      expect(configured).toMatchObject({ configured: true });
    }
    const first = await add('In this disposable project, use write_file to create done.txt containing exactly done followed by a newline. This one file is the entire task. Do not run commands or tests, read other files, commit, or use the network. Report what you actually did.');
    await waitResult(first, 'done');
    const done = await snapshot('done');
    const firstWorkspace = done.workspaces!.find(workspace => workspace.trackId === first)!.path;
    expect(await readFile(path.join(firstWorkspace, 'done.txt'), 'utf8')).toBe('done\n');
    expect((await git(['status', '--porcelain'])).stdout).toBe('');
    let proposal = page!.locator('[data-attention-card^="proposal-"]');
    await expect(proposal).toContainText('is done', { timeout: 60_000 });
    const log = await page!.evaluate(id => window.intercat.getEditorLog({ trackId: id, before: null }), first);
    expect(log.entries.some(entry => entry.scene === 'fork')).toBe(true);
    expect(log.entries.filter(entry => entry.scene === 'completed')).toHaveLength(1);
    observations.firstLog = log;
    const firstProposal = done.proposal!.id;
    await stopApp(); await launch();
    await expect.poll(async () => (await status()).proposal?.id).toBe(firstProposal);
    proposal = page!.locator('[data-attention-card^="proposal-"]');
    await expect(proposal).toContainText('is done', { timeout: 60_000 });
    await expect(page!.locator('.session-header')).not.toContainText('Checking privacy', { timeout: 60_000 });
    for (const zoom of [1, 2]) {
      await app!.evaluate(({ BrowserWindow }, factor) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(factor); }, zoom);
      await proposal.getByText('Details', { exact: true }).click();
      await expect(proposal).toContainText('Worker report:', { timeout: 60_000 });
      await expect(proposal.locator('.privacy-pending')).toHaveCount(0, { timeout: 60_000 });
      const review = proposal.getByRole('button', { name: 'Review for commit', exact: true });
      await review.scrollIntoViewIfNeeded(); await expect(review).toBeInViewport({ ratio: 1 });
      await writeFile(path.join(output, `native-proposal-${zoom * 100}.png`), Buffer.from(await app!.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
      await proposal.getByText('Details', { exact: true }).click();
    }
    await proposal.getByRole('button', { name: 'Review for commit', exact: true }).click();
    await expect(page!.locator('.commit-sheet')).toBeVisible({ timeout: 30_000 });
    await expect.poll(async () => (await status()).integration?.tracks.map(track => track.id), { timeout: 30_000 }).toContain(first);
    const review = await snapshot('review');
    expect(review.integration?.tracks.map(track => track.id)).toContain(first);
    await page!.getByRole('button', { name: 'Hide commit review', exact: true }).click();
    await app!.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(1));
    const second = await add('これは使い捨てプロジェクトの2段階の仕事です。全体の完了条件はfirst.txtとsecond.txtを両方作ることです。今回の処理予算はファイル追加1回だけなので、write_fileでfirst.txtへ「first」と改行を書いてターンを終えてください。second.txtへ「second」と改行を書く作業が残りますが、必要な情報はすべて揃っており、質問・確認・許可待ちはありません。残りは後続ターンで進められます。今回は全体が未完了なのでResultの種類をpartialとして残りを報告し、Waiting for youの行や相手への依頼・質問は付けないでください。コマンドやテストの実行、他のファイルの読取、コミット、ネットワーク利用は不要です。');
    await waitResult(second, 'partial');
    const partial = await snapshot('partial');
    const secondWorkspace = partial.workspaces!.find(workspace => workspace.trackId === second)!.path;
    expect(await readFile(path.join(secondWorkspace, 'first.txt'), 'utf8')).toBe('first\n');
    await expect(readFile(path.join(secondWorkspace, 'second.txt'))).rejects.toMatchObject({ code: 'ENOENT' });
    const secondSession = partial.workers!.find(worker => worker.trackId === second)!.sessionId;
    const pending = partial.proposal!.id;
    await expect(proposal).toContainText('finished part of it', { timeout: 60_000 });
    attempted++; await saveBudget();
    await proposal.getByRole('button', { name: 'Ask B to continue', exact: true }).click();
    await waitResult(second, 'done');
    const continued = await snapshot('continued');
    expect(await readFile(path.join(secondWorkspace, 'second.txt'), 'utf8')).toBe('second\n');
    expect(continued.workers!.find(worker => worker.trackId === second)!.sessionId).toBe(secondSession);
    expect(continued.workers!.find(worker => worker.trackId === second)!.output?.split('\n')[0]).toMatch(/[ぁ-んァ-ヶ一-龯]/);
    expect(continued.tracks!.find(track => track.id === second)!.completion?.result?.reason).toMatch(/[ぁ-んァ-ヶ一-龯]/);
    expect(continued.proposal!.id).not.toBe(pending);
    const secondLog = await page!.evaluate(id => window.intercat.getEditorLog({ trackId: id, before: null }), second);
    expect(secondLog.entries.filter(entry => entry.scene === 'completed')).toHaveLength(2);
    observations.secondLog = secondLog;
    await proposal.getByRole('button', { name: 'Not now', exact: true }).click();
    await expect.poll(async () => (await status()).proposal).toBeNull();
    await stopApp(); await launch();
    await expect.poll(async () => (await status()).proposal).toBeNull();
    const restoredLog = await page!.evaluate(id => window.intercat.getEditorLog({ trackId: id, before: null }), second);
    expect(restoredLog.entries.filter(entry => entry.scene === 'completed')).toEqual(secondLog.entries.filter(entry => entry.scene === 'completed'));
    passed = true;
  } catch (error) {
    primaryFailure = true;
    throw error;
  } finally {
    if (page && app) { try { await snapshot('final'); } catch { /* Startup or teardown failed. */ } }
    await writeFile(path.join(output, 'observations.json'), JSON.stringify({ passed, attemptedPaidTurns: attempted, cumulative: prior + attempted, observations }, null, 2));
    for (const file of ['editor-requests.jsonl', 'daemon/daemon.log', 'daemon/editor.log']) {
      try { await writeFile(path.join(output, path.basename(file)), await readFile(path.join(data, file))); } catch { /* Early failures may not create a log. */ }
    }
    try { await stopApp(); }
    catch (error) {
      await writeFile(path.join(output, 'cleanup-pending.json'), JSON.stringify({ data, daemon, error: String(error) }, null, 2));
      if (!primaryFailure) throw error;
    }
    if (!daemon) await rm(data, { recursive: true, force: true });
  }
});
