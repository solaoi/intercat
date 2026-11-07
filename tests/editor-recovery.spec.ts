import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, mkdir, rm } from 'node:fs/promises';
import path from 'node:path';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-editor-ui-');
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${data}`] });
  await mockDrafts(app);
  await app.firstWindow();
});
test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });

async function fixture(phase: string) {
  await app.evaluate(({ ipcMain, BrowserWindow }, phase) => {
    const state = { connected: true, sequence: 1, tracks: [], workers: [], utterances: [], editor: { phase, process: false, accepting: false, modelLoaded: false, generation: 1, restart: 3, canRetry: true, detail: 'Editor model file is missing or damaged: config.json', download: null as unknown } };
    const memory = globalThis as typeof globalThis & { editorState: typeof state; actions: { kind: string; id?: string }[]; rejectRetry: boolean };
    memory.editorState = state; memory.actions = []; memory.rejectRetry = true;
    const reply = (phase: string) => { state.editor.phase = phase; state.editor.canRetry = false; state.sequence++; return state.editor; };
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:editor-retry'); ipcMain.handle('intercat:editor-retry', (_event, id) => {
      memory.actions.push({ kind: 'retry', id });
      if (memory.rejectRetry) { memory.rejectRetry = false; throw new Error('Connection interrupted'); }
      return reply('starting');
    });
    ipcMain.removeHandler('intercat:editor-download'); ipcMain.handle('intercat:editor-download', (_event, id) => { memory.actions.push({ kind: 'download', id }); state.editor.download = { received: 5e9, total: 19.4e9, phase: 'downloading', file: 'weights' }; return reply('downloading'); });
    ipcMain.removeHandler('intercat:editor-pause'); ipcMain.handle('intercat:editor-pause', () => { memory.actions.push({ kind: 'pause' }); return reply('download_paused'); });
    ipcMain.removeHandler('intercat:editor-install'); ipcMain.handle('intercat:editor-install', () => { memory.actions.push({ kind: 'install' }); return reply('downloading'); });
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, phase);
  return app.firstWindow();
}

test('an exhausted Editor shows its failure and retries the same operation after a lost response', async () => {
  const page = await fixture('unavailable');
  const editor = page.getByRole('region', { name: 'Local Editor' });
  await expect(editor).toContainText('Editor unavailable · words wait · Stop still works');
  await editor.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(editor.getByRole('alert')).toContainText('Connection interrupted');
  await editor.getByRole('button', { name: 'Retry', exact: true }).click();
  await expect(editor).toContainText('Editor starting · words wait');
  const actions = await app.evaluate(() => (globalThis as typeof globalThis & { actions: { id: string }[] }).actions);
  expect(actions).toHaveLength(2); expect(actions[0].id).toBe(actions[1].id);
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toBeEditable();
});

test('model details, repair, pause and resume remain reachable at 200 percent', async () => {
  const page = await fixture('unavailable');
  const editor = page.getByRole('region', { name: 'Local Editor' });
  await editor.getByRole('button', { name: 'Details', exact: true }).click();
  const details = page.getByRole('dialog', { name: 'Editor details' });
  await expect(details).toContainText('config.json');
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2));
  await expect(details.getByRole('button', { name: 'Download again', exact: true })).toBeInViewport();
  await mkdir('research/verify/editor-recovery-20261104', { recursive: true });
  await app.evaluate(async ({ BrowserWindow }, file) => { const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage(); process.getBuiltinModule('fs').writeFileSync(file, image.toPNG()); }, path.resolve('research/verify/editor-recovery-20261104/details-200.png'));
  await details.getByRole('button', { name: 'Download again', exact: true }).click();
  await expect(editor.getByRole('progressbar', { name: 'Editor download' })).toHaveAttribute('value', '5000000000');
  await editor.getByRole('button', { name: 'Pause download', exact: true }).click();
  await expect(editor).toContainText('Editor download paused');
  await editor.getByRole('button', { name: 'Resume download', exact: true }).click();
  await expect(editor).toContainText('Downloading Editor');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { actions: { kind: string }[] }).actions.map(x => x.kind))).toEqual(['download', 'pause', 'download']);
});

test('first installation explains the local model download before starting it', async () => {
  const page = await fixture('not_configured');
  const editor = page.getByRole('region', { name: 'Local Editor' });
  await expect(editor).toContainText('Hugging Face (19.4 GB)');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { actions: unknown[] }).actions)).toEqual([]);
  await editor.getByRole('button', { name: 'Download Glimmer', exact: true }).click();
  await expect(editor).toContainText('Downloading Editor');
});

test('approval answers and the Stop shortcut remain available while Full privacy waits for the Editor', async () => {
  const page = await fixture('unavailable');
  await app.evaluate(({ ipcMain }) => {
    const memory = globalThis as typeof globalThis & { editorState: { sequence: number; tracks: unknown[]; workers: unknown[] }; workerActions: string[] };
    memory.workerActions = [];
    Object.assign(memory.editorState, { privacy: { mode: 'full', revision: 1 }, privacyRevision: 1 });
    ipcMain.removeHandler('intercat:privacy-preview'); ipcMain.handle('intercat:privacy-preview', () => { throw new Error('Full privacy is waiting for the local Editor.'); });
    memory.editorState.tracks = [{ id: 'a', label: 'A', title: 'A local task', status: 'working' }];
    memory.editorState.workers = [{ trackId: 'a', phase: 'approval', sessionId: 'session', activeTurn: 'turn', detail: null, prompts: [], checkpoint: null, checkpointPending: false, output: null, approvals: [{ id: 'approval', requirement: {}, subject: 'node local-test.cjs', reason: { kind: 'shell', message: 'Run a local test' }, canAllow: true, canDeny: true }] }];
    memory.editorState.sequence++;
    ipcMain.removeHandler('intercat:worker-answer'); ipcMain.handle('intercat:worker-answer', () => { memory.workerActions.push('answer'); });
    ipcMain.removeHandler('intercat:worker-stop'); ipcMain.handle('intercat:worker-stop', () => { memory.workerActions.push('stop'); });
  });
  await page.getByRole('button', { name: 'Allow once', exact: true }).click();
  const details = page.getByRole('region', { name: 'Local Editor' }).getByRole('button', { name: 'Details', exact: true });
  if (!await details.isVisible()) await page.getByRole('button', { name: /^\+\d+ more/ }).click();
  await details.click();
  await page.getByRole('dialog', { name: 'Editor details' }).press('Meta+.');
  await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { workerActions: string[] }).workerActions)).toEqual(['answer', 'stop']);
});
