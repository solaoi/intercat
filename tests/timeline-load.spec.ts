import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { benchmarkFrames } from './support/timeline-benchmark';
import type { DaemonStatus, TimelineClip, TimelineMark, WorkerState } from '../src/bridge';
import type { Application, Container, NineSliceSprite } from 'pixi.js';
import { nextVoiceOverTrack, requireVoiceOver, spokenTrack } from './support/voiceover';

type BenchmarkGlobal = typeof globalThis & { timelineBenchmarkApp?: Application };

async function visibleRunningBars(page: import('@playwright/test').Page) {
  return page.evaluate(() => {
    const app = (globalThis as BenchmarkGlobal).timelineBenchmarkApp!;
    const bars: { id: number; width: number; y: number }[] = [];
    function visit(container: Container) {
      for (const child of container.children) {
        const sprite = child as NineSliceSprite;
        if (sprite.renderPipeId === 'nineSliceSprite') {
          const bounds = sprite.getBounds();
          if (bounds.minY >= 0 && bounds.maxY <= app.screen.height && bounds.minX >= 0 && bounds.maxX <= app.screen.width) bars.push({ id: sprite.uid, width: sprite.width, y: bounds.minY });
        }
        visit(child);
      }
    }
    visit(app.stage);
    return bars.sort((a, b) => a.id - b.id);
  });
}

function fixture(trackCount: number, clipCount: number): { state: DaemonStatus; clips: TimelineClip[]; marks: TimelineMark[] } {
  const now = Date.now();
  const tracks = Array.from({ length: trackCount }, (_, i) => ({ id: `load-${i}`, label: String(i + 1), title: `利用者認証と変更履歴を確認するトラック ${i + 1}`, status: i < 100 ? 'working' : 'done' }));
  const clips: TimelineClip[] = tracks.flatMap((track, row) => Array.from({ length: clipCount }, (_, i) => ({ id: `${track.id}-${i}`, trackId: track.id, repairRunId: null, ordinal: i + 1, sessionId: `session-${row}`, turnId: `turn-${row}-${i}`, category: 'step', label: `検証 ${i + 1}：日本語の表示と操作を確認する`, kind: i % 3 === 0 ? 'think' : 'edit', start: now - 380000 + i * 1200, end: now - 379000 + i * 1200, running: row < 100 && i === clipCount - 1, generationEnd: now - 379500 + i * 1200, approximate: false, toolTicks: [] })));
  const workers: WorkerState[] = tracks.slice(0, 100).map(track => ({ trackId: track.id, phase: 'working', sessionId: `session-${track.id}`, identity: null, detail: null, activeTurn: 'active', output: null, checkpointPending: false, prompts: [], checkpoint: null, approvals: [] }));
  const marks = Array.from({ length: 600 }, (_, i) => ({ id: `input-${i}`, trackId: tracks[i % tracks.length].id, repairRunId: null, at: now - 370000 + i * 600, inputId: `input-${i}`, text: `過去の入力 ${i + 1}：画面外のトラックも含めて表示する` }));
  return { clips, marks, state: { connected: true, sequence: 1, tracks, workers, utterances: [], workspaces: [], timeline: { clock: { epochMs: now, monoMs: 0, bootId: 'load', timelineMs: now }, summaries: [] } } };
}

test('300 tracks remain drawable and reachable without a full-height canvas', async () => {
  test.skip(!process.env.INTERCAT_TIMELINE_LOAD, 'native graphics load test');
  const voiceover = process.env.INTERCAT_VOICEOVER === '1';
  test.setTimeout(voiceover ? 600000 : 90000);
  if (voiceover) await requireVoiceOver();
  const output = path.resolve(process.env.INTERCAT_TIMELINE_OUTPUT ?? 'research/verify/timeline-load-20261104');
  await mkdir(output, { recursive: true });
  const data = await mkdtemp(path.join(tmpdir(), 'intercat-timeline-load-'));
  let app: ElectronApplication | undefined;
  const errors: string[] = [];
  try {
    app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${data}`] });
  await mockDrafts(app);
    const page = await app.firstWindow();
    page.on('pageerror', error => errors.push(error.message));
    await page.addInitScript(() => {
      globalThis.__PIXI_APP_INIT__ = app => { (globalThis as BenchmarkGlobal).timelineBenchmarkApp = app as Application; };
    });
    await app.evaluate(({ ipcMain, BrowserWindow }, value) => {
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({ ...value.state, timeline: { ...value.state.timeline!, clock: { ...value.state.timeline!.clock, timelineMs: Date.now() } } }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', (_event, query) => ({ revision: 1, reset: query.since === null, removed: [], marks: query.since === 1 ? [] : value.marks.filter(mark => mark.at >= query.start && mark.at <= query.end), removedMarks: [], clips: query.since === 1 ? [] : value.clips.filter(clip => query.tracks.includes(clip.trackId) && clip.start <= query.end && (clip.running || clip.end >= query.start)) }));
      const window = BrowserWindow.getAllWindows()[0];
      window.setSize(1440, 1000); window.reload();
    }, fixture(300, 300));
    await expect(page.locator('.timeline')).toHaveAttribute('data-ready', 'true');
    const inputGroups = page.getByLabel('Timeline requests', { exact: true }).getByRole('button');
    await expect.poll(() => inputGroups.count()).toBeGreaterThan(0);
    expect(await inputGroups.evaluateAll(buttons => buttons.reduce((count, button) => count + Number(button.getAttribute('aria-label')?.match(/^(\d+) requests/)?.[1] ?? 1), 0))).toBe(600);
    const geometry = await page.evaluate(() => {
      const canvas = document.querySelector('.canvas-host canvas') as HTMLCanvasElement;
      const stage = document.querySelector('.stage')!;
      return { canvasHeight: canvas.height, viewportHeight: stage.clientHeight, dpr: devicePixelRatio, rows: document.querySelectorAll('.track-grid [role=row]').length };
    });
    await writeFile(path.join(output, 'geometry.json'), JSON.stringify({ ...geometry, errors }, null, 2));
    expect(geometry.canvasHeight).toBeLessThanOrEqual(Math.ceil(geometry.viewportHeight * geometry.dpr));
    const grid = page.getByRole('grid', { name: 'Tracks' });
    if (voiceover) await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].focus());
    await grid.focus();
    const spokenRows: { index: number; phrase: string }[] = [];
    for (let i = 0; i < 300; i++) {
      if (voiceover) await nextVoiceOverTrack(app);
      else await page.keyboard.press('ArrowDown');
      const position = await grid.evaluate(element => {
        const selected = document.getElementById(element.getAttribute('aria-activedescendant') ?? '');
        const viewport = document.querySelector('.timeline-scroll')!.getBoundingClientRect();
        const row = selected?.getBoundingClientRect();
        return { index: selected?.getAttribute('aria-rowindex'), visible: !!row && row.top >= viewport.top - 1 && row.bottom <= viewport.bottom + 1 };
      });
      expect(position).toEqual({ index: String(i + 1), visible: true });
      if (voiceover) {
        spokenRows.push({ index: i + 1, phrase: await spokenTrack(app, i + 1) });
        await writeFile(path.join(output, 'voiceover-progress.json'), JSON.stringify(spokenRows, null, 2));
      }
    }
    await expect(grid).toHaveAttribute('aria-activedescendant', 'track-load-299');
    await expect(page.locator('#track-load-299')).toBeInViewport();
    await expect(page.locator('#track-load-299 [role=gridcell]')).toHaveAccessibleName('300 · 利用者認証と変更履歴を確認するトラック 300 · done · result unclear');
    await page.screenshot({ path: path.join(output, 'last-track.png') });
    if (voiceover) {
      await writeFile(path.join(output, 'voiceover.json'), JSON.stringify({ rows: spokenRows, errors }, null, 2));
      expect(errors).toEqual([]);
      return;
    }
    await page.locator('.timeline-scroll').evaluate(element => { element.scrollTop = 0; });
    await expect(page.locator('#track-load-299')).toHaveCount(1);
    await grid.press('Home');
    await page.waitForTimeout(1500);
    const detail = await benchmarkFrames(page);
    await writeFile(path.join(output, 'detail-trace.json'), JSON.stringify(detail.trace));
    const { trace: _detailTrace, ...detailResult } = detail;
    await page.getByRole('button', { name: 'Overview', exact: true }).click();
    await expect(page.locator('.timeline')).toHaveAttribute('data-overview', 'true');
    await page.waitForTimeout(1000);
    const overview = await benchmarkFrames(page);
    await writeFile(path.join(output, 'overview-trace.json'), JSON.stringify(overview.trace));
    const { trace: _overviewTrace, ...overviewResult } = overview;
    await page.screenshot({ path: path.join(output, 'overview.png') });
    // A smaller browser scale exposes all 100 live rows at once. This checks
    // actual animated geometry rather than only off-screen running records.
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(.25));
    await expect.poll(() => page.locator('.timeline-scroll').evaluate(element => element.clientHeight)).toBeGreaterThanOrEqual(1200);
    await page.waitForTimeout(1500);
    const visibleActive = await page.locator('.track-grid [role=row]').evaluateAll(rows => rows.filter(row => {
      const box = row.getBoundingClientRect(), viewport = document.querySelector('.timeline-scroll')!.getBoundingClientRect();
      return Number(row.getAttribute('aria-rowindex')) <= 100 && box.top >= viewport.top && box.bottom <= viewport.bottom;
    }).length);
    expect(visibleActive).toBe(100);
    const allActiveGeometry = await page.locator('.timeline').evaluate(element => {
      const canvas = element.querySelector('canvas')!;
      return { width: element.clientWidth, height: element.clientHeight, canvasWidth: canvas.width, canvasHeight: canvas.height, dpr: devicePixelRatio };
    });
    expect(allActiveGeometry.canvasHeight).toBeLessThanOrEqual(Math.ceil(allActiveGeometry.height * allActiveGeometry.dpr));
    const barsBefore = await visibleRunningBars(page);
    expect(barsBefore).toHaveLength(100);
    const allActive = await benchmarkFrames(page, 720);
    const barsAfter = await visibleRunningBars(page);
    expect(barsAfter.map(bar => bar.id)).toEqual(barsBefore.map(bar => bar.id));
    const growingBars = barsAfter.filter((bar, index) => bar.width > barsBefore[index].width);
    expect(growingBars).toHaveLength(100);
    await writeFile(path.join(output, 'all-active-trace.json'), JSON.stringify(allActive.trace));
    const { trace: _allActiveTrace, ...allActiveResult } = allActive;
    await writeFile(path.join(output, 'performance.json'), JSON.stringify({ trackCount: 300, clipsPerTrack: 300, inputMarkers: 600, animatedTracks: 100, visibleActive, growingBars: growingBars.length, barsBefore, barsAfter, allActiveGeometry, detail: detailResult, overview: overviewResult, allActive: allActiveResult }, null, 2));
    await page.screenshot({ path: path.join(output, 'all-active.png') });
    expect(errors).toEqual([]);
    for (const result of [detail, overview, allActive]) {
      expect(result.fps).toBeGreaterThanOrEqual(60);
      expect(result.jsP95Ms).toBeLessThanOrEqual(2);
      expect(result.jsMaxMs).toBeLessThanOrEqual(2);
    }
  } finally {
    if (app) await closeWindow(app);
    await rm(data, { recursive: true, force: true });
  }
});
