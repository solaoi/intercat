import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import type { StepDetail, TimelineClip } from '../src/bridge';

let root: string;
let app: ElectronApplication;
let projection: { timeline: { revision: number; clips: TimelineClip[] }; details: StepDetail[] };

test.beforeAll(async () => {
  test.setTimeout(120000);
  root = await mkdtemp(path.join(tmpdir(), 'intercat-snapshot-ui-'));
  const fixture = path.join(root, 'fixture.json');
  const output = path.join(root, 'projection.json');
  execFileSync('python3', ['tools/build_msp_snapshot_fixture.py', fixture]);
  execFileSync('cargo', ['build', '-p', 'intercat-daemon', '--example', 'msp_snapshot'], { stdio: 'pipe' });
  execFileSync('target/debug/examples/msp_snapshot', [fixture, output]);
  const result = JSON.parse(await readFile(output, 'utf8')) as { cases: ({ id: string } & typeof projection)[] };
  projection = result.cases.find(value => value.id === 'late-snapshot')!;
});
test.afterAll(async () => { if (app) await closeWindow(app); await rm(root, { recursive: true, force: true }); });

test('snapshot-only history opens with real projected content and no fabricated turn or timing', async ({}, testInfo) => {
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${path.join(root, 'electron')}`] });
  const page = await app.firstWindow();
  const clip = projection.timeline.clips[0];
  expect(clip.category).toBe('snapshot');
  expect(clip.turnId).toBeNull();
  await app.evaluate(({ ipcMain, BrowserWindow }, value) => {
    const now = Date.now();
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status', () => ({ connected: true, sequence: 1, workerConfigured: true,
      tracks: [{ id: 'track', label: 'A', title: 'Recovered work', status: 'done' }], utterances: [], workers: [],
      timeline: { summaries: [], clock: { epochMs: now, monoMs: 1000, bootId: 'fixture', timelineMs: now } } }));
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => value.timeline);
    ipcMain.removeHandler('intercat:clip-detail');
    ipcMain.handle('intercat:clip-detail', (_event, id) => value.details.find(detail => detail.clip.id === id));
    BrowserWindow.getAllWindows()[0].setSize(1100, 800);
  }, projection);
  const button = page.getByRole('button', { name: `Open step A:${clip.ordinal}: Recovered history`, exact: true });
  await expect(button).toBeAttached();
  await button.focus(); await button.press('Enter');
  const detail = page.getByLabel('Step details');
  await expect(detail).toContainText('Recovered history · timing unavailable');
  await expect(detail).toContainText('Session history');
  await expect(detail).toContainText('suffix');
  await expect(detail).not.toContainText('stale high revision');
  await expect(detail).not.toContainText('Turn null');
  await expect(detail).not.toContainText('Finished · 0.0 s');
  await page.screenshot({ path: testInfo.outputPath('snapshot-history.png') });
  await writeFile(testInfo.outputPath('native-projection.json'), JSON.stringify(projection));
});
