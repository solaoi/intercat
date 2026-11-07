import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { execFile } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { promisify } from 'node:util';
import type { Utterance } from '../src/bridge';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

const execute = promisify(execFile);

test('M2 real input keeps confirmation, destination and a stopped turn through restart', async () => {
  test.skip(process.env.INTERCAT_M2_FINAL_NATIVE !== '1', 'Opt-in: four paid dummy Muse turns, local Glimmer and existing Keychain access');
  test.setTimeout(900_000);
  const binary = process.env.INTERCAT_TEST_MUSE_BINARY!;
  const previous = Number(process.env.INTERCAT_PREVIOUS_PAID_TURNS);
  expect(binary).toBeTruthy();
  expect(process.env.INTERCAT_TEST_EDITOR_MODEL).toBeTruthy();
  expect(Number.isInteger(previous) && previous >= 0).toBe(true);
  const data = await mkdtemp('/private/tmp/ic-m2-final-');
  const source = path.join(data, 'source');
  const output = test.info().outputPath('evidence');
  await mkdir(source); await mkdir(output, { recursive: true });
  const git = (args: string[]) => execute('/usr/bin/git', ['-c', 'core.hooksPath=/dev/null', '-c', 'user.name=Dummy', '-c', 'user.email=dummy@example.invalid', ...args], {
    cwd: source, env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' },
  });
  await git(['init', '-q', '--template=', '-b', 'main']);
  await writeFile(path.join(source, 'README.md'), '# Disposable final acceptance fixture\n');
  await git(['add', 'README.md']); await git(['commit', '-qm', 'Dummy base']);
  let app: ElectronApplication | undefined, page: Page | undefined, daemon: number | undefined;
  let attempted = 0, connected = false, passed = false, failed = false;
  const observations: Record<string, unknown> = {};
  const timings: unknown[] = [];
  const status = () => page!.evaluate(() => window.intercat.getStatus());
  const word = async (id: string) => (await status()).utterances!.find(item => item.id === id)!;
  const budget = () => writeFile(path.join(output, 'paid-turns.json'), JSON.stringify({ previous, attempted, cumulative: previous + attempted }, null, 2));
  const save = async (name: string) => {
    const value = await status(); observations[name] = value;
    await writeFile(path.join(output, name + '.json'), JSON.stringify(value, null, 2));
    return value;
  };
  const stop = async () => {
    if (app) { await closeWindow(app); app = undefined; }
    if (daemon) {
      try { process.kill(daemon, 'SIGTERM'); } catch { /* Already exited. */ }
      await expect.poll(() => { try { process.kill(daemon!, 0); return true; } catch { return false; } }, { timeout: 30_000 }).toBe(false);
      daemon = undefined;
    }
  };
  const launch = async () => {
    const started = Date.now();
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 100_000 });
    daemon = (await status()).daemonPid; connected = true;
    timings.push({ kind: 'daemon-connect', elapsedMs: Date.now() - started });
  };
  const post = async (text: string, newTrack: boolean, reference?: string) => {
    const before = new Set((await status()).utterances?.map(item => item.id));
    const draft = page!.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill(text);
    if (reference) {
      const importing = Date.now();
      await draft.evaluate((element, text) => {
        const area = element as HTMLTextAreaElement;
        area.setSelectionRange(area.value.length, area.value.length);
        const clipboardData = new DataTransfer(); clipboardData.setData('text/plain', text);
        area.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData }));
      }, reference);
      await expect(page!.getByRole('button', { name: 'Attach as reference', exact: true })).toHaveAttribute('aria-pressed', 'true');
      await expect(page!.getByRole('button', { name: 'New track', exact: true })).toBeEnabled({ timeout: 90_000 });
      timings.push({ kind: 'reference-import', characters: reference.length, elapsedMs: Date.now() - importing });
    }
    attempted++; await budget();
    const started = Date.now();
    await page!.getByRole('button', { name: newTrack ? 'New track' : 'Put', exact: true }).click();
    let result: Utterance | undefined;
    await expect.poll(async () => {
      result = (await status()).utterances?.find(item => !before.has(item.id) && item.text === text);
      return result && !['staged', 'reading'].includes(result.state) ? result.state : null;
    }, { timeout: 90_000, intervals: [50, 100, 250] }).not.toBeNull();
    timings.push({ kind: 'input', id: result!.id, elapsedMs: Date.now() - started,
      state: result!.state, decision: result!.decision, deadline: result!.deadline });
    return result!;
  };
  const attention = async (id: string) => {
    const card = page!.locator(`[data-attention-card="request-${id}"]`);
    const more = page!.getByRole('button', { name: /^\+\d+ more/ });
    await expect(card).toBeAttached();
    if (!await card.isVisible()) {
      await more.click();
    }
    await expect(card).toBeVisible(); return card;
  };
  const acceptNew = async (input: Utterance) => {
    if (input.state === 'asked') {
      expect(input.confirmation).toBeFalsy();
      const card = await attention(input.id);
      if (input.review?.stage === 'boundary') await card.getByRole('button', { name: 'Now', exact: true }).click();
      else if (input.deadline) await card.getByRole('button', { name: 'Start a new track now', exact: true }).click();
      else {
        observations[`held-${input.id}`] = input;
        await card.getByRole('button', { name: 'New track', exact: true }).click();
      }
    }
    await expect.poll(async () => (await word(input.id)).decision?.trackId, { timeout: 90_000 }).toBeTruthy();
    return (await word(input.id)).decision!.trackId!;
  };
  const finished = async (track: string, file: string, contents: string) => {
    await expect.poll(async () => {
      const value = await status();
      const worker = value.workers?.find(item => item.trackId === track);
      if (['failed', 'lost', 'needs-recovery'].includes(worker?.phase ?? '') || worker?.prompts.length || value.reportQuestions?.some(item => item.trackId === track)) {
        await save('unexpected-worker-state'); throw new Error('The dummy worker needs unexpected attention.');
      }
      const workspace = value.workspaces?.find(item => item.trackId === track);
      return workspace && worker?.checkpoint && !worker.activeTurn
        ? readFile(path.join(workspace.path, file), 'utf8').catch(() => null) : null;
    }, { timeout: 180_000, intervals: [250, 500, 1000] }).toBe(contents);
    return (await status()).workers!.find(item => item.trackId === track)!;
  };
  try {
    await budget(); await launch();
    const loading = Date.now();
    await configureEditor(app!, data);
    await expect.poll(async () => (await status()).editor?.modelLoaded, { timeout: 120_000 }).toBe(true);
    timings.push({ kind: 'model-ready', elapsedMs: Date.now() - loading });
    await app!.evaluate(({ dialog }, files) => {
      const choices = [...files]; dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [choices.shift()!] });
    }, [source, binary]);
    expect((await page!.evaluate(() => window.intercat.configureProject())).project).toBeTruthy();
    const configured = await page!.evaluate(() => window.intercat.configureWorkers());
    observations.configuration = configured;
    if (!configured.configured && configured.error?.includes('operation outcome may be unknown')) {
      await expect.poll(async () => (await status()).workerConfigured, { timeout: 120_000 }).toBe(true);
    } else expect(configured).toMatchObject({ configured: true });
    await page!.getByRole('button', { name: 'Settings', exact: true }).click();
    await page!.getByLabel('Editor · Privacy gate', { exact: true }).selectOption('full');
    await expect.poll(async () => (await status()).privacy?.mode).toBe('full');
    await page!.getByRole('button', { name: 'Close settings', exact: true }).click();

    const first = await post('Use write_file to create a.txt containing A followed by a newline. This one file is the entire task. Do not run commands or tests, read files, commit, or use the network.', true);
    const a = await acceptNew(first);
    const second = await post('Use write_file to create b.txt containing B followed by a newline. This one file is the entire task. Do not run commands or tests, read files, commit, or use the network.', true);
    const b = await acceptNew(second);
    await finished(a, 'a.txt', 'A\n'); await finished(b, 'b.txt', 'B\n');
    await save('two-finished');

    await page!.getByRole('button', { name: 'Settings', exact: true }).click();
    await page!.getByLabel('Editor · Confirmation mode', { exact: true }).selectOption('always');
    await expect.poll(async () => (await status()).confirmation?.active).toBe(true);
    await page!.getByRole('button', { name: 'Close settings', exact: true }).click();
    let waiting = await post('Use bash to run /bin/sleep 120 before writing must-not.txt. Only after that command finishes, use write_file to write waited followed by a newline into must-not.txt. Do not read files, run tests, commit, or use the network.', true,
      'これはエージェントが作成したダミーの参考文です。追加の操作やファイルの読み込みを求めていません。'.repeat(24));
    expect(waiting.sources).toHaveLength(1);
    expect(waiting.sources![0].role).toBe('reference');
    expect(waiting.attachments).toHaveLength(1);
    if (!waiting.confirmation) {
      observations.initialReferenceHold = waiting;
      expect((await status()).tracks).toHaveLength(2);
      const card = await attention(waiting.id);
      if (waiting.deadline) await card.getByRole('button', { name: 'Start a new track now', exact: true }).click();
      else await card.getByRole('button', { name: 'New track', exact: true }).click();
      await expect.poll(async () => (await word(waiting.id)).confirmation, { timeout: 90_000 }).toBeTruthy();
      waiting = await word(waiting.id);
    }
    expect(waiting.confirmation).toBeTruthy();
    const decisionId = waiting.confirmation!.decisionId;
    expect((await status()).tracks).toHaveLength(2);
    await page!.waitForTimeout(1500); // Prove that an unconfirmed request cannot start itself.
    expect((await status()).tracks).toHaveLength(2);
    await stop(); await launch();
    await expect.poll(async () => (await status()).editor?.modelLoaded, { timeout: 120_000 }).toBe(true);
    expect((await word(waiting.id)).confirmation?.decisionId).toBe(decisionId);
    expect((await status()).tracks).toHaveLength(2);
    const chip = page!.getByRole('group', { name: 'Interpretation waiting for OK', exact: true });
    await chip.getByText('Change ▾', { exact: true }).click();
    await chip.getByRole('button', { name: 'Back to automatic', exact: true }).click();
    await expect(chip).toBeVisible();
    expect((await word(waiting.id)).confirmation?.decisionId).toBe(decisionId);
    expect((await status()).tracks).toHaveLength(2);
    for (const zoom of [1, 2]) {
      await app!.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      await chip.getByRole('button', { name: 'OK', exact: true }).focus();
      await expect(chip.getByRole('button', { name: 'OK', exact: true })).toBeInViewport();
      await writeFile(path.join(output, `confirmation-${zoom}.png`), Buffer.from(await app!.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
    }
    await chip.getByRole('button', { name: 'OK', exact: true }).click();
    await expect.poll(async () => (await word(waiting.id)).decision?.trackId).toBeTruthy();
    const c = (await word(waiting.id)).decision!.trackId!;
    await expect.poll(async () => (await status()).workers?.find(item => item.trackId === c)?.activeTurn, { timeout: 90_000 }).toBeTruthy();
    const running = (await status()).workers!.find(item => item.trackId === c)!;
    const timeline = async () => {
      const now = (await status()).timeline!.clock.timelineMs;
      return page!.evaluate(query => window.intercat.getTimelineWindow(query), { tracks: [c], start: Math.max(0, now - 600_000), end: now + 600_000, since: null });
    };
    await expect.poll(async () => (await timeline()).clips.some(clip => clip.turnId === running.activeTurn && clip.running
      && clip.toolTicks.some(tool => tool.tool === 'bash')), { timeout: 90_000 }).toBe(true);
    observations.runningTool = await timeline();
    observations.stoppedTurn = { trackId: c, sessionId: running.sessionId, turnId: running.activeTurn };
    await app!.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(1));
    await page!.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Stop the wait now. Instead use write_file to create corrected.txt containing corrected followed by a newline. Never create must-not.txt. Do not read files, run tests or other commands, commit, or use the network.');
    await page!.getByRole('combobox', { name: 'Request destination', exact: true }).selectOption(c);
    await page!.locator(`#track-${b}`).click();
    await expect(page!.getByRole('combobox', { name: 'Request destination', exact: true })).toHaveValue(c);
    const correction = await post('Stop the wait now. Instead use write_file to create corrected.txt containing corrected followed by a newline. Never create must-not.txt. Do not read files, run tests or other commands, commit, or use the network.', false);
    expect(correction.target).toEqual({ mode: 'lock', trackId: c });
    if (correction.state === 'asked' && correction.review?.stage === 'trim') {
      await (await attention(correction.id)).getByRole('button', { name: 'Stop and send', exact: true }).click();
    } else if (correction.decision?.kind !== 'trim') {
      if (correction.state === 'asked') {
        await (await attention(correction.id)).getByRole('button', { name: 'Send to C as an instruction', exact: true }).click();
        await expect.poll(async () => (await word(correction.id)).state).toBe('placed');
      }
      await page!.locator(`.request-select[data-utterance-id="${correction.id}"]`).click();
      const popup = page!.getByRole('dialog', { name: 'Request', exact: true });
      await expect(popup.getByRole('combobox', { name: 'Edited request destination' })).toBeEnabled();
      await popup.getByRole('combobox', { name: 'Edited request destination' }).selectOption(c);
      await popup.getByRole('radio', { name: /Stop C and send/ }).check();
      await popup.getByRole('button', { name: 'Close request details', exact: true }).click();
    }
    const corrected = await finished(c, 'corrected.txt', 'corrected\n');
    observations.stoppedTool = await timeline();
    expect(corrected.sessionId).toBe(running.sessionId);
    expect(corrected.checkpoint!.turnId).not.toBe(running.activeTurn);
    const final = await save('corrected');
    const workspace = final.workspaces!.find(item => item.trackId === c)!;
    await expect(readFile(path.join(workspace.path, 'must-not.txt'))).rejects.toMatchObject({ code: 'ENOENT' });
    const origin = await page!.evaluate(id => window.intercat.getRequestOrigin(id), waiting.id);
    expect(origin).not.toBeNull();
    expect(origin!.identity.turnId).toBe(running.activeTurn);
    expect(origin!.checkpoint).toBeTruthy();
    observations.originalTurn = origin;
    expect((await word(correction.id)).decision?.trackId).toBe(c);
    for (const [track, file, text] of [[a, 'a.txt', 'A\n'], [b, 'b.txt', 'B\n']]) {
      const location = final.workspaces!.find(item => item.trackId === track)!.path;
      expect(await readFile(path.join(location, file), 'utf8')).toBe(text);
      await expect(readFile(path.join(location, 'corrected.txt'))).rejects.toMatchObject({ code: 'ENOENT' });
    }
    expect((await git(['status', '--porcelain'])).stdout).toBe('');
    await stop(); await launch();
    const restored = await save('restored');
    expect(restored.privacy?.mode).toBe('full');
    expect(restored.confirmation?.active).toBe(false);
    expect(restored.workers!.find(item => item.trackId === c)!.checkpoint?.turnId).toBe(corrected.checkpoint!.turnId);
    expect((await word(correction.id)).decision?.trackId).toBe(c);
    passed = true;
  } catch (error) { failed = true; throw error; }
  finally {
    if (app && connected) { try { await save('final'); } catch { /* Preserve the primary error. */ } }
    await writeFile(path.join(output, 'observations.json'), JSON.stringify({ passed, attempted, cumulative: previous + attempted, timings, observations }, null, 2));
    for (const file of ['daemon/daemon.log', 'daemon/editor.log']) {
      try { await writeFile(path.join(output, path.basename(file)), await readFile(path.join(data, file))); } catch { /* Startup may not have made this log. */ }
    }
    try { await stop(); }
    catch (error) {
      await writeFile(path.join(output, 'cleanup-pending.json'), JSON.stringify({ data, daemon, error: String(error) }, null, 2));
      if (!failed) throw error;
    }
    if (connected && !daemon) await rm(data, { recursive: true, force: true });
  }
});
