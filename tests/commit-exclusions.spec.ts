import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { IntegrationJob, IntegrationRequest } from '../src/commit/types';

for (const zoom of [1, 2]) test(`conflict exclusions show their scope and survive a lost response at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-commit-exclusions-');
  const app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}`] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const window = BrowserWindow.getAllWindows()[0];
      window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
      const labels = ['A', 'B', 'C', 'D', 'E', 'B2'];
      const tracks = labels.map((label, index) => ({ id: `01a1a200-0000-7000-8000-00000000000${index + 2}`, label,
        title: `${label}のダミー変更と継承した内容を確認する`, included: true, checkpointCount: 1, lastClip: 1, checkpoint: { sha: 'a'.repeat(40) } }));
      const source = tracks[1].id, children = [tracks[2].id, tracks[3].id, tracks[5].id];
      const job: IntegrationJob = { id: '01a1a200-0000-7000-8000-000000000001', revision: 1, status: 'merge-conflict',
        sessionName: 'Dummy inheritance', slug: 'dummy-inheritance', project: { id: 'dummy-folder', name: 'Dummy project', root: '/fixture', path: '/fixture', commonDir: '/fixture/.git', base: 'a'.repeat(40), baseBranch: 'main' },
        error: null, tracks, candidate: null, messages: {}, keptSecrets: [], contributions: {}, assumptions: [], assumptionsChanged: false, assumptionsPending: 0, assumptionsFailed: 0,
        exclusions: [{ track: source, dependents: children, omitted: false }],
        plan: { targetRef: 'refs/heads/intercat/dummy-inheritance', tree: null, rows: tracks.map((track, index) => ({ track: track.id, state: index === 0 ? 'included' : index === 1 ? 'conflict' : 'waiting', changes: [], conflicts: index === 1 ? [{ path: 'src/ダミー.ts', withTracks: [tracks[0].id] }] : [] })) } };
      const state = { connected: true, sequence: 1, integration: job, project: { id: 'dummy-project', name: 'Dummy project', folders: [job.project] }, trackFolders: Object.fromEntries(tracks.map(track => [track.id, job.project.id])), tracks: tracks.map(track => ({ ...track, status: 'done' })), workspaces: [], workers: [], utterances: [] };
      const memory = { state, calls: [] as IntegrationRequest[], applied: new Map<string, IntegrationJob>(), saves: 0 };
      Object.assign(globalThis, { commitExclusions: memory });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request: IntegrationRequest) => {
        memory.calls.push(request);
        const saved = memory.applied.get(request.opId); if (saved) return saved;
        if (request.action.action === 'select') {
          if (request.action.track !== source || request.action.included) throw new Error('Wrong dummy exclusion');
          job.tracks.forEach(track => { if (track.id === source || children.includes(track.id)) track.included = false; });
          job.exclusions[0].omitted = true;
          job.revision++; state.sequence++; memory.saves++;
          job.status = 'open'; job.plan!.tree = 'b'.repeat(40);
          job.plan!.rows.forEach(row => { row.state = job.tracks.find(track => track.id === row.track)!.included ? 'included' : 'excluded'; row.conflicts = []; });
        } else if (request.action.action === 'prepare') { job.status = 'checked'; state.sequence++; }
        memory.applied.set(request.opId, structuredClone(job));
        if (zoom === 2 && request.action.action === 'select' && memory.saves === 1) throw new Error('Dummy response lost after saving');
        return job;
      });
    }, zoom);
    const page = await app.firstWindow(); await page.reload();
    await page.getByRole('button', { name: 'Commit', exact: true }).click();
    const sheet = page.getByRole('complementary', { name: 'Commit review', exact: true });
    const scope = sheet.getByText('Leaving B out also leaves C, D, B2 out (inherits B)', { exact: true });
    await expect(scope).toBeVisible(); await scope.scrollIntoViewIfNeeded(); await expect(scope).toBeInViewport();
    const row = sheet.locator('.commit-track').filter({ has: page.getByRole('checkbox', { name: 'Include Worker B', exact: true }) });
    await row.getByRole('button').click();
    const leave = sheet.getByRole('button', { name: 'Leave B out of this commit', exact: true });
    await leave.scrollIntoViewIfNeeded(); await expect(leave).toBeInViewport();
    const before = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`exclusion-${zoom}.png`), Buffer.from(before, 'base64'));
    await leave.focus(); await leave.press('Enter');
    if (zoom === 2) await sheet.getByRole('button', { name: 'Retry action', exact: true }).click();
    for (const label of ['B', 'C', 'D', 'B2']) await expect(sheet.getByRole('checkbox', { name: `Include Worker ${label}`, exact: true })).not.toBeChecked();
    for (const label of ['C', 'D', 'B2']) await expect(sheet.getByRole('checkbox', { name: `Include Worker ${label}`, exact: true })).toBeDisabled();
    for (const label of ['A', 'E']) await expect(sheet.getByRole('checkbox', { name: `Include Worker ${label}`, exact: true })).toBeChecked();
    await expect(sheet.getByRole('checkbox', { name: 'Include Worker B', exact: true })).toBeEnabled();
    await page.reload(); await page.getByRole('button', { name: 'Commit', exact: true }).click();
    await expect(sheet.getByRole('checkbox', { name: 'Include Worker C', exact: true })).toBeDisabled();
    const result = await app.evaluate(() => {
      const memory = (globalThis as typeof globalThis & { commitExclusions: { saves: number; calls: IntegrationRequest[] } }).commitExclusions;
      return { saves: memory.saves, selections: memory.calls.filter(call => call.action.action === 'select') };
    });
    expect(result.saves).toBe(1);
    if (zoom === 2) expect(result.selections[0].opId).toBe(result.selections[1].opId);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
