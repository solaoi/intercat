import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { DaemonStatus, Folder } from '../src/bridge';
import type { IntegrationJob, IntegrationRequest } from '../src/commit/types';

for (const zoom of [1, 2]) test(`Folder choice survives a lost reply and Commit selects one folder at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-folder-ui-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const folders: Folder[] = ['A', 'B'].map(letter => ({ id: `folder-${letter}`, name: `Dummy ${letter}`, path: `/dummy/${letter}`, root: `/dummy/${letter}`, commonDir: `/dummy/${letter}/.git`, base: 'a'.repeat(40), baseBranch: 'main' }));
      const tracks = ['A', 'B'].map(letter => ({ id: `track-${letter}`, label: letter, title: `Dummy ${letter} work`, status: 'waiting' }));
      const state: DaemonStatus = { connected: true, sequence: 1, workerConfigured: true, project: { id: 'project', name: 'Dummy project', folders },
        tracks, workers: [], workspaces: [], utterances: [], trackFolders: { 'track-B': 'folder-A' } };
      const memory = { state, choices: [] as { opId: string; trackId: string; folderId: string }[], commits: [] as IntegrationRequest[] };
      Object.assign(globalThis, { folderFixture: memory });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:folder-assign'); ipcMain.handle('intercat:folder-assign', (_event, request) => {
        memory.choices.push(request);
        if (memory.choices.length === 1) throw new Error('Saved reply lost');
        state.trackFolders!['track-A'] = request.folderId;
        state.workspaces = tracks.map(track => ({ trackId: track.id, path: `/dummy/${track.id}`, reference: '', base: '', attachmentRoots: [], state: 'ready', error: null }));
        state.sequence!++;
      });
      ipcMain.removeHandler('intercat:integration-command'); ipcMain.handle('intercat:integration-command', (_event, request: IntegrationRequest) => {
        memory.commits.push(request);
        if (request.action.action !== 'open') throw new Error('Only Open is expected');
        const selected = tracks.filter(track => request.action.action === 'open' && request.action.tracks?.includes(track.id));
        const folder = folders.find(folder => folder.id === state.trackFolders![selected[0].id])!;
        const job: IntegrationJob = { id: 'review', revision: 1, status: 'open', slug: 'dummy', sessionName: 'Dummy review', project: folder,
          tracks: selected.map(track => ({ ...track, included: true, checkpointCount: 1, lastClip: null, checkpoint: { sha: 'b'.repeat(40) } })),
          plan: null, error: null, candidate: null, messages: {}, keptSecrets: [], contributions: {}, exclusions: [], assumptions: [], assumptionsPending: 0, assumptionsFailed: 0, assumptionsChanged: false };
        state.integration = job; state.sequence!++; return job;
      });
      BrowserWindow.getAllWindows()[0].setSize(1040, 760);
      BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    }, zoom);
    const page = await app.firstWindow();
    const capture = async (name: string) => {
      const image = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(info.outputPath(name), Buffer.from(image, 'base64'));
    };
    const choice = page.locator('.folder-choice');
    await expect(choice).toContainText('Choose a folder for Worker A');
    await expect(choice.getByRole('button', { name: 'Dummy B', exact: true })).toBeEnabled();
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toBeEnabled();
    await capture('folder-choice.png');
    await choice.getByRole('button', { name: 'Dummy B', exact: true }).click();
    await expect(choice).toContainText('Saved reply lost');
    await choice.getByRole('button', { name: 'Retry folder selection' }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).trackFolders?.['track-A']).toBe('folder-B');
    await page.getByRole('textbox', { name: 'Request draft' }).focus();
    await page.mouse.move(5, 5);
    await expect(choice).toHaveCount(0);
    await page.getByRole('button', { name: 'Commit', exact: true }).first().click();
    const review = page.getByRole('complementary', { name: 'Commit review' });
    await expect(review).toContainText('Choose a folder to review');
    const select = review.getByRole('button', { name: 'Review Dummy B', exact: true });
    await expect(select).toBeInViewport({ ratio: 1 });
    await capture('folder-picker.png');
    await select.click();
    await expect(review.locator('.commit-heading').getByText('Dummy B', { exact: true })).toBeVisible();
    await expect(review.getByRole('checkbox', { name: 'Include Worker A' })).toBeVisible();
    await expect(review.getByRole('checkbox', { name: 'Include Worker B' })).toHaveCount(0);
    const captured = await app.evaluate(() => (globalThis as typeof globalThis & { folderFixture: { choices: unknown[]; commits: IntegrationRequest[] } }).folderFixture);
    expect(captured.choices).toHaveLength(2); expect(captured.choices[0]).toEqual(captured.choices[1]);
    expect(captured.commits).toHaveLength(1);
    expect(captured.commits[0].action).toEqual({ action: 'open', tracks: ['track-A'] });
    await capture('folder-review.png');
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
