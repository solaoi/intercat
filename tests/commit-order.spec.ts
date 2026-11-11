import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { DaemonStatus } from '../src/bridge';
import type { IntegrationJob, IntegrationRequest } from '../src/commit/types';

for (const zoom of [1, 2]) {
  test(`commit order precedes the review and survives a lost acknowledgement at ${zoom * 100}%`, async () => {
    const directory = await mkdtemp('/private/tmp/ic-commit-order-');
    const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
    try {
      await mockDrafts(app);
      await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
        const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
        const tracks = ['A', 'B'].map((label, index) => ({ id: `01a1a200-0000-7000-8000-00000000000${index + 2}`, label,
          title: `${label}：キャッシュと同じファイルの変更を確認するダミー作業`, included: true, checkpointCount: 1, lastClip: 1, checkpoint: { sha: 'a'.repeat(40) } }));
        const job: IntegrationJob = { id: '01a1a200-0000-7000-8000-000000000001', revision: 1, status: 'ordering',
          sessionName: 'Order fixture', slug: 'order-fixture', project: { id: 'dummy-folder', name: 'Dummy project', root: '/fixture', path: '/fixture', commonDir: '/fixture/.git', base: 'a'.repeat(40), baseBranch: 'main' },
          error: null, tracks, candidate: null, messages: {}, keptSecrets: [], contributions: {}, pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions: [], assumptions: [], assumptionsChanged: false, assumptionsPending: 0, assumptionsFailed: 0,
          plan: { undoConflict:null,targetRef: 'refs/heads/intercat/order-fixture', tree: null, rows: tracks.map(track => ({ track: track.id, state: 'waiting', changes: [], conflicts: [] })) },
          orderReview: { order: tracks.map(track => track.id), confirmed: false, overlaps: [{ path: 'src/キャッシュ/shared.ts', tracks: tracks.map(track => track.id) }] } };
        const state: DaemonStatus & { sequence: number } = { connected: true, sequence: 1, integrations: [job], project: { id: 'dummy-project', name: job.project.name, folders: [job.project] }, trackFolders: Object.fromEntries(tracks.map(track => [track.id,job.project.id])),
          tracks: tracks.map(track => ({ ...track, status: 'done' })), workspaces: tracks.map(track => ({ trackId: track.id, state: 'ready', path: '/fixture', reference: '', base: '', deniedRoots: [], attachmentRoots: [], error: null })), workers: [], utterances: [] };
        const memory = { state, calls: [] as IntegrationRequest[], applied: new Set<string>(), orders: 0, lose: zoom === 2 };
        Object.assign(globalThis, { commitOrderFixture: memory });
        ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
        ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
        ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request: IntegrationRequest) => {
          memory.calls.push(request);
          if (memory.applied.has(request.opId)) return state.integrations![0];
          if (request.action.action === 'order') {
            const ids = request.action.tracks;
            job.tracks = ids.map(id => job.tracks.find(track => track.id === id)!);
            job.revision++; job.status = 'open'; job.orderReview = { ...job.orderReview!, order: ids, confirmed: true };
            job.plan!.tree = 'b'.repeat(40); memory.orders++; state.sequence++; memory.applied.add(request.opId);
            if (memory.lose) { memory.lose = false; throw new Error('Dummy lost acknowledgement'); }
          } else if (request.action.action === 'prepare') {
            job.status = 'checked'; state.sequence++;
          }
          return state.integrations![0];
        });
      }, zoom);
      const page = await app.firstWindow(); await page.reload();
      await page.getByRole('button', { name: 'Commit', exact: true }).click();
      const proposal = page.getByRole('complementary', { name: 'Commit order proposal', exact: true });
      await expect(proposal).toBeVisible();
      await expect(page.getByRole('complementary', { name: 'Commit review', exact: true })).toHaveCount(0);
      await proposal.getByRole('button', { name: 'Hide commit order proposal' }).click();
      await page.getByRole('button', { name: 'Commit', exact: true }).click();
      await expect(proposal).toBeVisible();
      await proposal.getByText('1 shared file', { exact: true }).click();
      await expect(proposal).toContainText('src/キャッシュ/shared.ts');
      await proposal.getByRole('button', { name: 'Change order', exact: true }).click();
      const earlier = proposal.getByRole('button', { name: 'Move Worker B earlier', exact: true });
      await earlier.focus(); await earlier.press('Enter');
      await expect(proposal.getByRole('list', { name: 'Commit order' }).locator('li').first()).toContainText('Worker B');
      const before = await app.evaluate(() => (globalThis as typeof globalThis & { commitOrderFixture: { calls: IntegrationRequest[] } }).commitOrderFixture.calls);
      expect(before.every(call => call.action.action === 'open')).toBe(true);
      const submit = proposal.getByRole('button', { name: 'Use this order', exact: true });
      await submit.scrollIntoViewIfNeeded(); await expect(submit).toBeInViewport();
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`order-${zoom}.png`), Buffer.from(pixels, 'base64'));
      await submit.focus(); await submit.press('Enter');
      if (zoom === 2) await page.getByRole('button', { name: /^Retry (action|order confirmation)$/ }).click();
      await expect(page.getByRole('complementary', { name: 'Commit review', exact: true })).toBeVisible();
      const result = await app.evaluate(() => {
        const memory = (globalThis as typeof globalThis & { commitOrderFixture: { calls: IntegrationRequest[]; orders: number; state: DaemonStatus } }).commitOrderFixture;
        return { calls: memory.calls, orders: memory.orders, tracks: memory.state.integrations![0]!.tracks.map(track => track.label) };
      });
      expect(result.orders).toBe(1); expect(result.tracks).toEqual(['B', 'A']);
      expect(result.calls.some(call => call.action.action === 'prepare')).toBe(true);
      const orders = result.calls.filter(call => call.action.action === 'order');
      if (zoom === 2) expect(orders[0].opId).toBe(orders[1].opId);
      expect(result.calls.some(call => call.action.action === 'commit')).toBe(false);
    } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
  });
}
