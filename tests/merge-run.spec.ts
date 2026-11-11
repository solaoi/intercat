import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { IntegrationJob, IntegrationRequest } from '../src/commit/types';
import type { RepairRun } from '../src/commit/repair';

for (const zoom of [1, 2]) test(`a conflict starts its merge and keeps work controls separate at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-merge-run-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const a = '01a1a200-0000-7000-8000-000000000002', b = '01a1a200-0000-7000-8000-000000000003';
      const tracks = [a, b].map((id, index) => ({ id, label: index ? 'B' : 'A', title: 'Disposable merge work', included: true, checkpointCount: 1, lastClip: 1, checkpoint: { sha: 'a'.repeat(40) } }));
      const job: IntegrationJob = { id: '01a1a200-0000-7000-8000-000000000001', revision: 1, status: 'merge-conflict',
        sessionName: 'Dummy merge', slug: 'dummy-merge', project: { id: 'dummy-folder', name: 'Dummy merge folder', root: '/fixture', path: '/fixture', commonDir: '/fixture/.git', base: 'a'.repeat(40), baseBranch: 'main' },
        error: null, tracks, candidate: null, messages: {}, keptSecrets: [], contributions: {}, assumptions: [], assumptionsChanged: false, assumptionsPending: 0, assumptionsFailed: 0, pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions: [],
        plan: { undoConflict:null,targetRef: 'refs/heads/intercat/dummy-merge', tree: null, rows: tracks.map((track, index) => ({ track: track.id, state: index ? 'conflict' : 'included', changes: [], conflicts: index ? [{ path: 'dummy.txt', withTracks: [a] }] : [] })) } };
      const run: RepairRun = { id: '01a1a200-0000-7000-8000-000000000004', revision: 8, owner:{kind:'commit',id:job.id,revision:1}, track: b, purpose: { kind: 'merge' }, phase: 'running', approvals: [], prompts: [], output: null, failure: null, session: 'dummy-merge-session', turn: 'dummy-merge-turn', secretInput: null, refresh: null, restoring: null, remaining: null };
      const worker = { trackId: b, sessionId: 'dummy-main-session', activeTurn: 'dummy-main-turn', phase: 'working', generation: 1, detail: null, checkpoint: null, prompts: [], approvals: [] };
      const state = { connected: true, sequence: 1, workerConfigured: true, integrations: [job], project: { id: 'dummy-project', name: 'Dummy merge', folders: [job.project] }, trackFolders: { [a]: job.project.id, [b]: job.project.id }, tracks: tracks.map(track => ({ ...track, status: 'working' })), workers: [worker], workspaces: [], repairRuns: [] as RepairRun[], utterances: [] };
      const fixture = { state, calls: [] as unknown[] };
      Object.assign(globalThis, { mergeRunFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request: IntegrationRequest) => {
        fixture.calls.push(request);
        if (request.action.action === 'prepare') {
          job.status = 'failed'; job.error = 'Dummy conflict'; state.repairRuns = [run]; state.sequence++;
          job.candidate = { revision: 1, state: 'conflict', buildRevision: 1, workspace: '/private/tmp/disposable-merge', head: 'c'.repeat(40), tree: 'd'.repeat(40), settingsHash: 'f'.repeat(64), commits: [], blockedCommit: null, undo:null,undoConflict:null,secretScans: [], check: null, reason: 'Dummy conflict', failedTrack: b, hooksSkipped: false, repair: null, dependencyRuns: [], conflict: { kind: 'overlay', base: 'a'.repeat(40), incoming: 'b'.repeat(40), paths: ['dummy.txt'] } };
        }
        return job;
      });
      ipcMain.removeHandler('intercat:repair-stop'); ipcMain.handle('intercat:repair-stop', (_event, request) => {
        if (request.id !== run.id) throw new Error('Do not stop the dummy work Run');
        fixture.calls.push(request); run.phase = 'held'; run.revision++; state.sequence++; return { accepted: true };
      });
      ipcMain.removeHandler('intercat:repair-control'); ipcMain.handle('intercat:repair-control', (_event, request) => {
        if (request.id !== run.id || request.action !== 'resume') throw new Error('Wrong dummy merge control');
        fixture.calls.push(request); run.phase = 'running'; run.revision++; state.sequence++; return { accepted: true };
      });
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow(); await page.reload();
    await page.getByRole('button', { name: 'Commit', exact: true }).click();
    const sheet = page.getByRole('complementary', { name: 'Commit review', exact: true });
    const panel = sheet.getByRole('region', { name: 'B · for the commit', exact: true });
    await expect(panel).toContainText('Resolving conflicts');
    const stop = panel.getByRole('button', { name: 'Stop merge', exact: true });
    await stop.scrollIntoViewIfNeeded(); await expect(stop).toBeInViewport(); await stop.click();
    await expect(panel).toContainText('Merge held');
    await sheet.getByRole('button', {name:/B.*Disposable merge work/}).click();
    const retry = sheet.getByRole('button', { name: 'Let B try again', exact: true });
    await retry.scrollIntoViewIfNeeded(); await expect(retry).toBeInViewport(); await retry.click();
    await expect(panel).toContainText('Resolving conflicts');
    const result = await app.evaluate(() => (globalThis as any).mergeRunFixture);
    expect(result.calls.filter((call: any) => call.action?.action === 'prepare')).toHaveLength(1);
    expect(result.state.workers[0]).toMatchObject({ activeTurn: 'dummy-main-turn', phase: 'working', sessionId: 'dummy-main-session' });
    expect(result.calls.filter((call: any) => typeof call.action !== 'object').map((call: any) => call.id)).toEqual([result.state.repairRuns[0].id, result.state.repairRuns[0].id]);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
