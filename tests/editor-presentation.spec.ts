import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { EditorAwayPage, EditorPresentation } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication, page: Page, data: string, now: number;
const line = () => page.getByRole('region', { name: 'Editor explanation', exact: true });
async function launch() {
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app); page = await app.firstWindow(); now = Date.now();
  await page.clock.install({ time: now }); await page.clock.pauseAt(now + 1000);
  await app.evaluate(({ ipcMain }, now) => {
    const fixture = { state: { connected: true, sequence: 1, session: { id: 'dummy-presentation', slug: 'dummy', name: 'Dummy presentation', createdAt: 1 },
      tracks: [{ id: 'A', label: 'A', title: 'Arithmetic', status: 'waiting' }, { id: 'B', label: 'B', title: 'Documentation', status: 'waiting' }], workers: [], utterances: [], editorEntries: [],
      timeline: { clock: { timelineMs: now, epochMs: now, monoMs: 1, bootId: 'dummy-boot' }, summaries: [] } },
      pages: {} as Record<string, EditorAwayPage>, queries: [] as any[], windows: [] as any[], fail: false };
    Object.assign(globalThis, { presentationFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => fixture.state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', (_event, query) => { fixture.windows.push(query); return { revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }; });
    ipcMain.removeHandler('intercat:editor-away'); ipcMain.handle('intercat:editor-away', (_event, query) => {
      fixture.queries.push(query); if (fixture.fail) { fixture.fail = false; throw new Error('Dummy unavailable log'); }
      return fixture.pages[String(query.before)];
    });
    ipcMain.removeHandler('intercat:editor-log'); ipcMain.handle('intercat:editor-log', () => ({ entries: [], before: null }));
  }, now);
  await page.clock.runFor(1100);
}
async function speech() {
  await app.evaluate((_e, now) => {
    const fixture = (globalThis as any).presentationFixture;
    fixture.state.editorEntries = [{ id: 1, scene: 'fork', trackId: 'A', inputId: null, title: 'Dummy track', line: 'New track for A.', detail: 'Created the dummy arithmetic track.', createdAt: now, expiresAt: now + 12_000 }];
    fixture.state.sequence++;
  }, await page.evaluate(() => Date.now()));
  await page.clock.runFor(1100);
}
async function present(state: EditorPresentation) {
  await app.evaluate(({ BrowserWindow }, state) => BrowserWindow.getAllWindows()[0].webContents.send('intercat:editor-presentation-changed', state), state);
  await page.clock.runFor(50);
}
async function returnFromAway(character: 'speak' | 'mute' = 'mute', fail = false) {
  await app.evaluate((_e, { now, fail }) => {
    const fixture = (globalThis as any).presentationFixture;
    fixture.fail = fail;
    const entry = { id: 51, scene: 'completed', trackId: 'A', inputId: null, title: 'Dummy turn', line: 'Worker A completed this turn.', detail: 'ダミーの元の説明。'.repeat(60), createdAt: now - 1000, expiresAt: 0 };
    fixture.pages.null = { updates: 51, clips: 3, effects: 1, waiting: ['A'], entries: [entry], before: 51 };
    fixture.pages['51'] = { updates: 51, clips: 3, effects: 1, waiting: ['A'], entries: [{ ...entry, id: 50, line: 'Earlier original speech.', detail: 'Kept with its original timestamp.' }], before: null };
  }, { now, fail });
  const state = { character, hidden: false, returned: { id: 'dummy-return', from: now - 900_000, to: now } } satisfies EditorPresentation;
  await present({ ...state, hidden: true, returned: null });
  await present(state);
  return state;
}
test.beforeEach(async () => { data = await mkdtemp('/private/tmp/ic-editor-presentation-'); await launch(); });
test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });

test('Mute immediately hides speech and its open details while preserving Take back Undo', async () => {
  await speech(); await expect(line()).toContainText('New track for A.');
  await line().getByRole('button', { name: 'more ›' }).click();
  await page.getByRole('button', { name: 'Settings' }).click();
  await page.getByLabel('Editor · Character').selectOption('mute');
  await page.getByRole('button', { name: 'Close settings' }).click();
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await expect(page.getByRole('dialog', { name: 'Editor explanation details' })).toHaveCount(0);
  await app.evaluate((_e, at) => {
    const fixture = (globalThis as any).presentationFixture;
    fixture.state.editHistory = { undo: { entryId: 'dummy-undo', label: 'Take back', text: 'Dummy request', at, blocked: null }, redo: null, takenBack: [] };
    fixture.state.sequence++;
  }, await page.evaluate(() => Date.now()));
  await page.clock.runFor(1100);
  await expect(line().getByRole('button', { name: 'Undo take back' })).toBeVisible();
  await page.getByRole('button', { name: 'Settings' }).click();
  await expect(page.getByLabel('Editor · Character')).toHaveValue('mute');
  await page.getByRole('button', { name: 'Close settings' }).click();
  await closeWindow(app); await launch(); await speech();
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await page.getByRole('button', { name: 'Settings' }).click();
  await expect(page.getByLabel('Editor · Character')).toHaveValue('mute');
});

for (const zoom of [1, 2]) test(`away summaries survive Mute and show timestamped originals at ${zoom * 100}%`, async () => {
  await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
  await page.getByRole('gridcell', { name: /^B · Documentation/ }).click();
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Dummy unsent request');
  const returned = await returnFromAway();
  await expect(line()).toContainText('While you were away');
  await line().getByRole('button', { name: 'more ›' }).click();
  const dialog = page.getByRole('dialog', { name: 'Editor explanation details' });
  await expect(dialog).toContainText('3 closed clips · 1 effect outside Git');
  await expect(dialog).toContainText('A waits for you.');
  await expect(dialog.locator('time')).toHaveCount(1);
  await dialog.getByRole('button', { name: 'Earlier explanations' }).click();
  await expect(dialog).toContainText('Earlier original speech.');
  await expect(dialog.locator('time')).toHaveCount(2);
  await page.clock.runFor(15_000);
  await expect(dialog).toBeVisible();
  await expect(dialog.getByRole('button', { name: 'Close Editor explanation' })).toBeInViewport();
  await writeFile(test.info().outputPath(`away-${zoom * 100}.png`), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
  await page.keyboard.press('Escape');
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeFocused();
  await present(returned); await page.clock.runFor(1100);
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  expect(await app.evaluate(() => (globalThis as any).presentationFixture.queries.length)).toBe(2);
  const bounds = await app.evaluate(() => (globalThis as any).presentationFixture.windows.at(-1));
  expect(bounds.end - bounds.start).toBeCloseTo(1_125_000, -1);
  await expect(page.locator('.track-row[aria-selected="true"]')).toContainText('Documentation');
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toHaveValue('Dummy unsent request');
});

test('a failed return read retries without losing the interval or duplicating the summary', async () => {
  await returnFromAway('speak', true);
  await expect(page.getByText('The away summary could not be loaded. Retrying…')).toBeVisible();
  await page.clock.runFor(5100);
  await expect(line()).toContainText('While you were away');
  await expect(page.getByText('The away summary could not be loaded. Retrying…')).toHaveCount(0);
  expect(await app.evaluate(() => (globalThis as any).presentationFixture.queries.map((q: any) => [q.from, q.to]))).toEqual([[now - 900_000, now], [now - 900_000, now]]);
});

test('hidden speech stays hidden and visible attention cards survive Mute', async () => {
  await present({ character: 'mute', hidden: true, returned: null }); await speech();
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await app.evaluate(() => {
    const fixture = (globalThis as any).presentationFixture;
    fixture.state.utterances = [{ id: 'dummy-question', text: 'Dummy ambiguous request', attachments: [], createdAt: 1, revision: 1, state: 'asked', target: { mode: 'none' }, decision: null, reason: 'Choose a track.' }];
    fixture.state.sequence++;
  });
  await present({ character: 'mute', hidden: false, returned: null }); await page.clock.runFor(1100);
  await expect(page.getByLabel('Read the full request', { exact: true })).toBeVisible();
  await expect(page.getByLabel('Read the full request', { exact: true })).toContainText('Dummy ambiguous request');
  await expect(line()).toHaveAttribute('data-speaking', 'false');
});

test('a second absence has a fresh lifetime after the first summary expires', async () => {
  const first = await returnFromAway();
  await expect(line()).toContainText('While you were away');
  await page.clock.runFor(15_000);
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await present({ ...first, hidden: true, returned: null });
  await present({ ...first, returned: { ...first.returned!, id: 'second-return' } });
  await expect(line()).toContainText('While you were away');
  await page.clock.runFor(1100);
  expect(await app.evaluate(() => (globalThis as any).presentationFixture.queries.length)).toBe(2);
});
