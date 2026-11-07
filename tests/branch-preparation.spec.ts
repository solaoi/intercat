import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

async function setup(options: { zoom?: number; fail?: boolean; failLoad?: boolean; slow?: boolean; empty?: boolean; recovery?: boolean; defer?: boolean; step?: boolean; unlocked?: boolean } = {}) {
  const directory = await mkdtemp('/private/tmp/ic-branch-preparation-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain, BrowserWindow }, options) => {
    const opId = '01960000-0000-7000-8000-000000000001';
    const receipt = '01960000-0000-7000-8000-000000000002';
    const point = { id: 30, turnId: 'turn-a', checkpoint: 'a'.repeat(40), endedAt: 1_730_000_000_000, timelineAt: 1000, shellWasRunning: true, available: true };
    const start = new Date(2026, 0, 1, 14, 0, 0).getTime();
    const clip = { id: 'dummy-step', trackId: 'A', repairRunId: null, ordinal: 1, sessionId: 'parent', turnId: 'turn-a', category: 'step', label: 'Dummy saved step', kind: 'edit', start: start + 60_000, end: start + 90_000, running: false, generationEnd: null, approximate: false, toolTicks: [] };
    const word = { id: 'branch-command', text: 'Branch from a saved turn', revision: 1, state: 'placed', createdAt: 1, attachments: [], target: { mode: 'lock', trackId: 'A' },
      decision: { kind: 'command', command: 'branch-from', trackId: 'A', boundary: null },
      presentation: { command: 'branch-from', operationId: opId, receiptId: receipt, trackId: options.unlocked ? null : 'A' } as object | null };
    const pending = { request: { opId, parentTrackId: 'A', turnId: point.turnId }, trackId: 'A2', parentSessionId: 'parent', sessionId: null as string | null, workspace: {}, checkpoint: {}, phase: 'preparing' };
    const state = { connected: true, sequence: 1, workerConfigured: true, workers: [], tracks: [{ id: 'A', label: 'A', title: 'ダミーの仕事', status: 'working' }, { id: 'B', label: 'B', title: '別のダミー', status: 'done' }], utterances: options.recovery || options.defer ? [] : [word], branches: options.recovery ? [pending] : [] };
    const fixture = { branchCalls: [] as object[], pointCalls: [] as object[], fail: !!options.fail, failLoad: !!options.failLoad, release: null as (() => void) | null, publish: () => { state.utterances.push(word); state.sequence++; } };
    Object.assign(globalThis, { branchFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:utterance-read'); ipcMain.handle('intercat:utterance-read', () => word);
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, request) => {
      if (request.opId !== receipt) throw new Error('Wrong display receipt');
      word.state = 'landed'; word.revision++; word.presentation = null; state.sequence++; return word;
    });
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: options.step ? [clip] : [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:clip-detail'); ipcMain.handle('intercat:clip-detail', () => ({ clip, items: [] }));
    ipcMain.removeHandler('intercat:branch-points'); ipcMain.handle('intercat:branch-points', async (_event, request) => {
      fixture.pointCalls.push(request);
      if (fixture.failLoad) { fixture.failLoad = false; throw new Error('Dummy point read failure'); }
      if (options.slow && request.trackId === 'A') await new Promise<void>(resolve => { fixture.release = resolve; });
      if (options.empty) return { points: [], before: null };
      if (request.trackId === 'B') return { points: [{ ...point, id: 40, turnId: 'turn-b', checkpoint: 'b'.repeat(40), shellWasRunning: false }], before: null };
      if (request.before !== null) return { points: [{ ...point, id: 10, turnId: 'older-turn', checkpoint: 'c'.repeat(40), endedAt: point.endedAt - 300_000 }], before: null };
      return { points: [point, { ...point, id: 20, turnId: 'invalid-turn', checkpoint: 'd'.repeat(40), available: false }], before: 20 };
    });
    ipcMain.removeHandler('intercat:branch'); ipcMain.handle('intercat:branch', (_event, request) => {
      fixture.branchCalls.push(request);
      if (fixture.fail) { fixture.fail = false; state.branches = [pending]; throw new Error('Lost branch acknowledgement'); }
      const result = { ...pending, request, sessionId: 'child', phase: 'ready' };
      state.branches = [result]; state.tracks.push({ id: 'A2', label: 'A2', title: 'Branch · ダミーの仕事', status: 'stopped' }); state.sequence++;
      return result;
    });
    const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(options.zoom ?? 1);
  }, options);
  const page = await app.firstWindow(); await page.reload();
  return { app, page, cleanup: async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); } };
}

test('a spoken branch only opens preparation; confirmation uses the selected old turn and keeps the draft', async () => {
  const { app, page, cleanup } = await setup({ defer: true });
  try {
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Keep this dummy draft.');
    await app.evaluate(() => (globalThis as any).branchFixture.publish());
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Branch from here', exact: true })).toBeDisabled();
    expect(await app.evaluate(() => (globalThis as any).branchFixture.branchCalls)).toEqual([]);
    await expect(dialog.getByText('End of A’s turn · aaaaaaaaaa · shell was running', { exact: true })).toBeVisible();
    await expect(dialog.getByRole('radio').nth(1)).toBeDisabled();
    await dialog.getByRole('button', { name: 'Load older turns', exact: true }).click();
    await expect(dialog.getByRole('radio')).toHaveCount(3);
    await dialog.getByRole('radio').nth(2).check();
    await dialog.getByRole('button', { name: 'Branch from here', exact: true }).click();
    await expect(dialog).toHaveCount(0);
    const calls = await app.evaluate(() => (globalThis as any).branchFixture.branchCalls);
    expect(calls).toEqual([{ opId: '01960000-0000-7000-8000-000000000001', parentTrackId: 'A', turnId: 'older-turn' }]);
    await expect(page.getByRole('combobox', { name: 'Request destination', exact: true })).toHaveValue('A2');
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Keep this dummy draft.');
  } finally { await cleanup(); }
});

test('a lost branch acknowledgement retries the same turn and operation', async () => {
  const { app, page, cleanup } = await setup({ fail: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await dialog.getByRole('radio').first().check();
    await dialog.getByRole('button', { name: 'Branch from here', exact: true }).click();
    await expect(dialog.getByRole('alert')).toContainText('Lost branch acknowledgement');
    await expect(dialog.getByRole('combobox', { name: 'Branch source worker' })).toBeDisabled();
    await expect(dialog.getByRole('radio').first()).toBeDisabled();
    await dialog.getByRole('button', { name: 'Retry branch', exact: true }).click();
    await expect(dialog).toHaveCount(0);
    const calls = await app.evaluate(() => (globalThis as any).branchFixture.branchCalls);
    expect(calls).toHaveLength(2); expect(calls[1]).toEqual(calls[0]);
  } finally { await cleanup(); }
});

test('an unlocked branch request requires a worker choice rather than using the viewed track', async () => {
  const { app, page, cleanup } = await setup({ unlocked: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect(dialog.getByRole('combobox', { name: 'Branch source worker' })).toHaveValue('');
    expect(await app.evaluate(() => (globalThis as any).branchFixture.pointCalls)).toEqual([]);
    await dialog.getByRole('combobox', { name: 'Branch source worker' }).selectOption('B');
    await dialog.getByRole('radio').check();
    await dialog.getByRole('button', { name: 'Branch from here', exact: true }).click();
    const calls = await app.evaluate(() => (globalThis as any).branchFixture.branchCalls);
    expect(calls[0].parentTrackId).toBe('B');
  } finally { await cleanup(); }
});

test('a branch command can use the open step preview without losing the selected point', async () => {
  const { app, page, cleanup } = await setup({ defer: true, step: true });
  try {
    await page.getByRole('button', { name: 'Open step A:1: Dummy saved step', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Close step preview' })).toBeVisible();
    await app.evaluate(() => (globalThis as any).branchFixture.publish());
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByRole('radio').first()).toBeChecked();
    expect(await app.evaluate(() => (globalThis as any).branchFixture.branchCalls)).toEqual([]);
    await dialog.getByRole('button', { name: 'Close branch preparation' }).click();
    await expect(page.getByRole('button', { name: 'Close step preview' })).toBeVisible();
  } finally { await cleanup(); }
});

test('an unsaved request prevents branching from changing its destination', async () => {
  const { app, page, cleanup } = await setup({ defer: true });
  try {
    await app.evaluate(({ ipcMain }) => {
      ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', () => { throw new Error('Dummy save failure'); });
    });
    await page.getByRole('combobox', { name: 'Request destination', exact: true }).selectOption('B');
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Keep this pending dummy request.');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect(page.getByRole('button', { name: /Retry save/ })).toBeVisible();
    await app.evaluate(() => (globalThis as any).branchFixture.publish());
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await dialog.getByRole('radio').first().check();
    await expect(dialog.getByText('Finish saving the current request before creating a branch.', { exact: true })).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Branch from here', exact: true })).toBeDisabled();
    expect(await app.evaluate(() => (globalThis as any).branchFixture.branchCalls)).toEqual([]);
    await dialog.getByRole('button', { name: 'Close branch preparation' }).click();
    await expect(page.getByRole('combobox', { name: 'Request destination', exact: true })).toHaveValue('B');
    await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Keep this pending dummy request.');
  } finally { await cleanup(); }
});

test('a delayed result for another worker never changes the selected branch point', async () => {
  const { app, page, cleanup } = await setup({ slow: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect.poll(() => app.evaluate(() => typeof (globalThis as any).branchFixture.release)).toBe('function');
    await dialog.getByRole('combobox', { name: 'Branch source worker' }).selectOption('B');
    await expect(dialog.getByRole('radio')).toHaveCount(1);
    await app.evaluate(() => (globalThis as any).branchFixture.release?.());
    await expect(dialog).toContainText('bbbbbbbbbb');
    await expect(dialog).not.toContainText('aaaaaaaaaa');
    await dialog.getByRole('radio').check();
    await dialog.getByRole('button', { name: 'Branch from here', exact: true }).click();
    const calls = await app.evaluate(() => (globalThis as any).branchFixture.branchCalls);
    expect(calls[0].parentTrackId).toBe('B'); expect(calls[0].turnId).toBe('turn-b');
  } finally { await cleanup(); }
});

test('a previously confirmed pending branch can finish after reopening the app', async () => {
  const { app, page, cleanup } = await setup({ recovery: true });
  try {
    await page.getByRole('button', { name: 'Retry branch preparation', exact: true }).click();
    await expect(page.getByRole('button', { name: 'Retry branch preparation', exact: true })).toHaveCount(0);
    const calls = await app.evaluate(() => (globalThis as any).branchFixture.branchCalls);
    expect(calls).toEqual([{ opId: '01960000-0000-7000-8000-000000000001', parentTrackId: 'A', turnId: 'turn-a' }]);
  } finally { await cleanup(); }
});

test('no checkpoint leaves branching unavailable', async () => {
  const { page, cleanup } = await setup({ empty: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect(dialog.getByText('no checkpoint yet', { exact: true })).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Branch from here', exact: true })).toBeDisabled();
    await dialog.getByRole('button', { name: 'Close branch preparation' }).click();
    await expect(dialog).toHaveCount(0);
  } finally { await cleanup(); }
});

test('a point read failure is not reported as an empty history and can be retried', async () => {
  const { page, cleanup } = await setup({ failLoad: true });
  try {
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect(dialog.getByRole('alert')).toContainText('Dummy point read failure');
    await expect(dialog.getByText('no checkpoint yet', { exact: true })).toHaveCount(0);
    await dialog.getByRole('button', { name: 'Retry loading turns', exact: true }).click();
    await expect(dialog.getByRole('radio')).toHaveCount(2);
    await expect(dialog.getByRole('alert')).toHaveCount(0);
  } finally { await cleanup(); }
});

for (const zoom of [1, 2]) test(`saved branch points and confirmation remain reachable at ${zoom * 100}%`, async () => {
  const { app, page, cleanup } = await setup({ zoom });
  try {
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await dialog.getByRole('radio').first().check();
    const confirm = dialog.getByRole('button', { name: 'Branch from here', exact: true });
    await confirm.focus(); await expect(confirm).toBeInViewport();
    await mkdir('research/verify/m2-branch-preparation-20261107', { recursive: true });
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(`research/verify/m2-branch-preparation-20261107/preparation-${zoom * 100}.png`, Buffer.from(pixels, 'base64'));
    await page.keyboard.press('Escape'); await expect(dialog).toHaveCount(0);
  } finally { await cleanup(); }
});

test('the native command opens preparation and reads that workers saved points without forking', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(240_000);
  const directory = await mkdtemp('/private/tmp/ic-native-branch-preparation-');
  await mkdir(directory + '/model');
  await writeFile(directory + '/python', '#!/usr/bin/python3\n' + await readFile('tests/fixtures/park_editor.py', 'utf8'), { mode: 0o700 });
  const app = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
  let pid: number | undefined;
  try {
    const page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await configureEditor(app, directory, directory + '/model', directory + '/python');
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60_000 });
    const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill('Dummy branch parent'); await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).tracks?.length).toBe(1);
    const track = (await page.evaluate(() => window.intercat.getStatus())).tracks![0];
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    const queued = await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Open branch', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.id === queued.id)?.presentation?.command).toBe('branch-from');
    await page.evaluate(async id => {
      const word = (await window.intercat.getStatus()).utterances!.find(word => word.id === id)!;
      await window.intercat.resolveUtterance({ opId: await window.intercat.createOperationId(), utteranceId: id, revision: word.revision, resolution: { action: 'hold' } });
    }, queued.id);
    await page.getByRole('button', { name: 'Close settings', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Branch from a saved turn', exact: true });
    await expect(dialog).toHaveCount(0);
    await page.getByRole('button', { name: 'Start request: Open branch', exact: true }).click();
    await expect(dialog).toBeVisible();
    await dialog.getByRole('combobox', { name: 'Branch source worker' }).selectOption(track.id);
    await expect(dialog.getByText('no checkpoint yet', { exact: true })).toBeVisible();
    await expect(dialog.getByRole('button', { name: 'Branch from here', exact: true })).toBeDisabled();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.text === 'Open branch')?.state).toBe('landed');
    const state = await page.evaluate(() => window.intercat.getStatus());
    expect(state.branches).toEqual([]); expect(state.tracks).toHaveLength(1);
    expect(state.utterances?.find(word => word.text === 'Open branch')?.state).toBe('landed');
    await mkdir('research/verify/m2-branch-preparation-20261107', { recursive: true });
    await writeFile('research/verify/m2-branch-preparation-20261107/native.json', JSON.stringify(state, null, 2));
  } finally {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    await rm(directory, { recursive: true, force: true });
  }
});
