import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

test('a generated description shows its full explanation, keeps the original request and survives restarting', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180_000);
  const data = await mkdtemp('/private/tmp/ic-description-ui-');
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
    const original = 'Add arithmetic tests, including the zero case.';
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill(original);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.[0]?.decision?.description?.title, { timeout: 30_000 }).toBe('Arithmetic tests');
    const state = await page.evaluate(() => window.intercat.getStatus());
    const word = state.utterances![0];
    expect(state.session!.name).toBe('Arithmetic tests');
    expect(state.tracks![0].title).toBe('Arithmetic tests');
    await expect(page.locator('.session-header h1')).toHaveText('Arithmetic tests');
    const line = page.getByRole('region', { name: 'Editor explanation', exact: true });
    await expect(line).toContainText('New track for A.');
    await line.getByRole('button', { name: 'more ›' }).click();
    const details = page.getByRole('dialog', { name: 'Editor explanation details' });
    await expect(details).toContainText(original);
    await app.evaluate(({ BrowserWindow }) => {
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2);
    });
    await expect(details.getByRole('button', { name: 'Close Editor explanation' })).toBeInViewport();
    const bounds = await details.boundingBox();
    const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width);
    expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(viewport.height);
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('description-200.png'), Buffer.from(pixels, 'base64'));
    await page.keyboard.press('Escape');
    await expect(details).toHaveCount(0);
    await page.locator(`[data-utterance-id="${word.id}"]`).click();
    const request = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(request.getByRole('region', { name: 'Recorded Editor explanation' })).toContainText(original);
    await expect(request).toContainText(original);
    await request.getByRole('button', { name: 'Close request details' }).click();
    const before = (await page.evaluate(() => window.intercat.getStatus())).utterances![0].decision!.description;
    const beforeLog = await page.evaluate(trackId => window.intercat.getEditorLog({ trackId, before: null }), state.tracks![0].id);
    expect(beforeLog.entries).toHaveLength(1);
    expect(beforeLog.entries[0].detail).toBe(before!.detail);

    await closeWindow(app);
    process.kill(pid!, 'SIGTERM');
    await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    app = await electron.launch({ args: ['.', '--user-data-dir=' + data] });
    page = await app.firstWindow();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.[0]?.decision?.description, { timeout: 95_000 }).toEqual(before);
    const restored = await page.evaluate(() => window.intercat.getStatus());
    pid = restored.daemonPid;
    expect(restored.utterances![0].text).toBe(original);
    expect(restored.session).toEqual(state.session);
    expect(restored.tracks).toEqual(state.tracks);
    expect(await page.evaluate(trackId => window.intercat.getEditorLog({ trackId, before: null }), state.tracks![0].id)).toEqual(beforeLog);

    await writeFile(test.info().outputPath('description-restored.json'), JSON.stringify(restored, null, 2));
  } finally {
    try {
      const page = await app.firstWindow();
      await writeFile(test.info().outputPath('final-status.json'), JSON.stringify(await page.evaluate(() => window.intercat.getStatus()), null, 2));
    } catch { /* The closed app has no final status. */ }
    try {
      await writeFile(test.info().outputPath('editor.log'), await readFile(path.join(data, 'daemon/editor.log')));
    } catch { /* A failure before configuring the editor has no model log. */ }
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* The isolated daemon may have already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    await rm(data, { recursive: true, force: true });
  }
});

test('an oversized generated line falls back while its Japanese explanation remains readable at 200 percent', async () => {
  const data = await mkdtemp('/private/tmp/ic-description-preview-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  try {
    const page = await app.firstWindow();
    await app.evaluate(({ ipcMain }) => {
      const state = { connected: true, sequence: 1,
        tracks: [{ id: 'A', label: 'A', title: '試験の追加', status: 'waiting' }],
        utterances: [{ id: 'description-preview', text: '元の依頼文', state: 'placed', revision: 1,
          target: { mode: 'none' }, attachments: [], createdAt: Date.now(), reason: null,
          decision: { kind: 'fork', trackId: 'A', boundary: null, description: {
            title: '生成した題', line: '長すぎる台詞'.repeat(30),
            detail: '判断が記録された後に作られた説明です。\n'.repeat(40), createdAt: Date.now(),
          } } }], workers: [] };
      Object.assign(state, { editorEntries: [{ id: 1, trackId: 'A', inputId: 'description-preview', scene: 'fork', title: '生成した題', line: state.utterances[0].decision.description.line, detail: state.utterances[0].decision.description.detail, createdAt: Date.now(), expiresAt: Date.now() + 12_000 }] });
      Object.assign(globalThis, { descriptionFixture: state });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    });
    await expect(page.locator('.editor-line')).toContainText('New track created.');
    await page.locator('.editor-line').getByRole('button', { name: 'more ›' }).click();
    await app.evaluate(({ BrowserWindow }) => {
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2);
    });
    const details = page.getByRole('dialog', { name: 'Editor explanation details' });
    await expect(details).toContainText('判断が記録された後に作られた説明です。');
    await expect(details.getByRole('button', { name: 'Close Editor explanation' })).toBeInViewport();
    const bounds = await details.boundingBox();
    const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
    expect(bounds!.width).toBeLessThanOrEqual(viewport.width);
    expect(bounds!.height).toBeLessThanOrEqual(viewport.height);
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width);
    expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(viewport.height);
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('description-japanese-200.png'), Buffer.from(pixels, 'base64'));
    await details.evaluate(element => { element.scrollTop = element.scrollHeight; });
    await expect(details.getByRole('button', { name: 'Close Editor explanation' })).toBeInViewport();
    await details.getByRole('button', { name: 'Close Editor explanation' }).click();
    await expect(details).toHaveCount(0);
    await expect(page.locator('.editor-line')).toHaveAttribute('data-speaking', 'false', { timeout: 15_000 });
    await expect(page.locator('[data-utterance-id="description-preview"]')).toContainText('生成した題');
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeFocused();
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { descriptionFixture: { sequence: number; editorEntries: { id: number; line: string; createdAt: number; expiresAt: number }[] } }).descriptionFixture;
      state.editorEntries[0].id++;
      state.editorEntries[0].line = 'W'.repeat(70);
      state.editorEntries[0].createdAt = Date.now();
      state.editorEntries[0].expiresAt = Date.now() + 12_000;
      state.sequence++;
    });
    await expect(page.locator('.editor-line p')).toHaveText('New track created.');
  } finally {
    await closeWindow(app);
    await rm(data, { recursive: true, force: true });
  }
});

for (const zoom of [1, 2]) test(`routine and unconfirmed interpretations show their titles without speaking at ${zoom * 100}%`, async () => {
  const data = await mkdtemp('/private/tmp/ic-quiet-titles-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const state = { connected: true, sequence: 1, tracks: [{ id: 'A', label: 'A', title: 'Cache work', status: 'waiting' }], workers: [],
        utterances: [{ id: 'quiet-title', text: 'An original dummy request', state: 'placed', revision: 1, target: { mode: 'none' }, attachments: [], createdAt: Date.now(), reason: null, confirmation: null as unknown,
          decision: { kind: 'extend', trackId: 'A' as string | null, boundary: null, command: null, description: { title: 'Generated extend title', line: 'This routine placement should stay quiet.', detail: 'The recorded interpretation remains readable.', createdAt: Date.now() } } }] };
      Object.assign(globalThis, { quietTitleFixture: state });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    });
    const page = await app.firstWindow();
    await page.reload();
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(zoom); }, zoom);
    for (const kind of ['extend', 'schedule', 'trim', 'park', 'ask', 'plan', 'command', 'fork']) {
      const title = `Generated ${kind} title`;
      await app.evaluate((_electron, value) => {
        const state = (globalThis as typeof globalThis & { quietTitleFixture: { sequence: number; utterances: { state: string; confirmation: unknown; decision: { kind: string; trackId: string | null; description: { title: string } } }[] } }).quietTitleFixture;
        const word = state.utterances[0];
        word.state = ['ask', 'plan', 'fork'].includes(value.kind) ? 'asked' : 'held';
        word.confirmation = value.kind === 'fork' ? { decisionId: 'pending-fork', turn: null } : null;
        word.decision.kind = value.kind;
        word.decision.trackId = ['ask', 'plan', 'fork'].includes(value.kind) ? null : 'A';
        word.decision.description.title = value.title;
        state.sequence++;
      }, { kind, title });
      await expect(page.getByText(title, { exact: false }).first()).toBeVisible();
      await expect(page.getByRole('region', { name: 'Editor explanation', exact: true })).toHaveAttribute('data-speaking', 'false');
      if (['ask', 'plan'].includes(kind)) await expect(page.getByRole('button', { name: 'Take back', exact: true })).toBeInViewport();
      if (kind === 'fork') await expect(page.getByRole('button', { name: 'OK', exact: true })).toBeInViewport();
    }
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { quietTitleFixture: { sequence: number; utterances: { state: string; confirmation: unknown; decision: { kind: string; description: { title: string } } }[] } }).quietTitleFixture;
      const word = state.utterances[0];
      Object.assign(word, { state: 'asked', confirmation: null, startWaiting: { candidates: [] } });
      word.decision.kind = 'command'; word.decision.description.title = 'Start queued review'; state.sequence++;
    });
    await expect(page.getByRole('group', { name: 'Start waiting work', exact: true })).toContainText('Start queued review');
    await expect(page.getByRole('button', { name: 'Read again', exact: true })).toBeInViewport();
    for (const invalid of ['', '   ', 'Invalid\nname', '題'.repeat(31)]) {
      await app.evaluate((_electron, invalid) => {
        const state = (globalThis as typeof globalThis & { quietTitleFixture: { sequence: number; utterances: { state: string; confirmation: unknown; decision: { kind: string; trackId: string | null; description: { title: string } } }[] } }).quietTitleFixture;
        const word = state.utterances[0];
        Object.assign(word, { state: 'held', confirmation: null, startWaiting: null });
        word.decision.kind = 'extend'; word.decision.trackId = 'A'; word.decision.description.title = invalid; state.sequence++;
      }, invalid);
      await expect(page.locator('[data-utterance-id="quiet-title"]')).toContainText('An original dummy request');
      await expect(page.locator('.request-generated-title')).toHaveCount(0);
    }
  } finally { await closeWindow(app); await rm(data, { recursive: true, force: true }); }
});
