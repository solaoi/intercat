import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, EditorEntry, EditorLogPage } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication; let page: Page; let data: string; let now: number;
function entry(id: number, scene: string, trackId = 'A', persistent = false): EditorEntry {
  return { id, trackId, scene, inputId: null, title: `Dummy ${scene} ${id}`, line: `${scene} ${id}.`, detail: `Recorded dummy ${scene} ${id}.`, createdAt: now + id, expiresAt: persistent ? null : now + 12_000 };
}
async function update(entries: EditorEntry[]) {
  await app.evaluate((_e, entries) => { const fixture = (globalThis as any).editorLogFixture; fixture.state.editorEntries = entries; fixture.state.sequence++; }, entries);
  await page.clock.runFor(1100);
}
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-editor-log-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app); page = await app.firstWindow();
  now = Date.now(); await page.clock.install({ time: now }); await page.clock.pauseAt(now + 1000);
  await app.evaluate(({ ipcMain }) => {
    const state: DaemonStatus = { connected: true, sequence: 1, session: { id: 'dummy-log', name: 'Editor log test', slug: 'dummy', createdAt: 1 },
      tracks: [{ id: 'A', label: 'A', title: 'Arithmetic', status: 'waiting' }, { id: 'B', label: 'B', title: 'Documentation', status: 'waiting' }],
      workers: [], utterances: [], editorEntries: [] };
    const fixture = { state, logs: {} as Record<string, EditorLogPage>, queries: [] as unknown[] };
    Object.assign(globalThis, { editorLogFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:editor-log'); ipcMain.handle('intercat:editor-log', (_event, query) => { fixture.queries.push(query); return fixture.logs[`${query.trackId}/${query.before}`] ?? { entries: [], before: null }; });
  });
  await page.clock.runFor(1100);
});
test.afterEach(async () => { await closeWindow(app); await rm(data, { recursive: true, force: true }); });
const line = () => page.getByRole('region', { name: 'Editor explanation', exact: true });

test('the reserved Editor row stays next to the composer before, during and after speech', async () => {
  const before = await page.locator('.composer').boundingBox();
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await update([entry(1, 'fork')]); await expect(line()).toContainText('fork 1.');
  expect((await page.locator('.composer').boundingBox())!.y).toBe(before!.y);
  await line().getByRole('button', { name: 'more ›' }).focus();
  await page.clock.runFor(12_000); await expect(line()).toHaveAttribute('data-speaking', 'false');
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeFocused();
  expect((await page.locator('.composer').boundingBox())!.y).toBe(before!.y);
});

test('new transient speech returns to the persistent notice after expiring', async () => {
  const lost = entry(1, 'lost', 'A', true);
  await update([lost]); await expect(line()).toContainText('lost 1.');
  await update([lost, entry(2, 'fork')]); await expect(line()).toContainText('fork 2.');
  await page.clock.runFor(12_000); await expect(line()).toContainText('lost 1.');
  await update([]); await expect(line()).toHaveAttribute('data-speaking', 'false');
});

test('persistent notices prefer the worker holding queued words', async () => {
  await app.evaluate(() => { const state = (globalThis as any).editorLogFixture.state; state.utterances = [{ id: 'waiting', text: 'Dummy follow-up', attachments: [], createdAt: 1, state: 'placed', revision: 1, target: { mode: 'none' }, reason: null, decision: { kind: 'extend', trackId: 'A', boundary: 'after-turn' } }]; });
  await update([entry(1, 'lost', 'A', true), entry(2, 'lost', 'B', true)]);
  await expect(line()).toContainText('lost 1.');
});

test('open details stay on their original explanation when a new notice arrives', async () => {
  await update([entry(1, 'lost', 'A', true)]);
  await line().getByRole('button', { name: 'more ›' }).click();
  await update([entry(1, 'lost', 'A', true), entry(2, 'fork')]);
  const dialog = page.getByRole('dialog', { name: 'Editor explanation details' });
  await expect(dialog).toContainText('Recorded dummy lost 1.');
  await page.keyboard.press('Escape'); await expect(line()).toContainText('fork 2.');
});

for (const zoom of [1, 2]) test(`long details and past explanations are readable at ${zoom * 100}%`, async () => {
  const long = { ...entry(1, 'fork'), line: 'Long '.repeat(30), detail: 'ダミーの説明です。'.repeat(80) };
  await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
  await update([long]); await expect(line()).toContainText('New track created.');
  await line().getByRole('button', { name: 'more ›' }).click();
  const dialog = page.getByRole('dialog', { name: 'Editor explanation details' });
  await expect(dialog).toContainText(long.detail);
  await expect(dialog.getByRole('button', { name: 'Close Editor explanation' })).toBeInViewport();
  await writeFile(test.info().outputPath(`editor-line-${zoom * 100}.png`), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
  await page.keyboard.press('Escape');
  await app.evaluate((_e, long) => { (globalThis as any).editorLogFixture.logs['A/null'] = { entries: [long], before: 1 }; (globalThis as any).editorLogFixture.logs['A/1'] = { entries: [{ ...long, id: 0, title: 'Older explanation', detail: 'Kept after taking the request back.' }], before: null }; }, long);
  await page.locator('.editor-log > summary').click(); await page.clock.runFor(100);
  const log = page.getByRole('region', { name: 'Editor log for Worker A' });
  await expect(log).toContainText(long.detail);
  await log.getByRole('button', { name: 'Earlier explanations' }).click();
  await expect(log).toContainText('Kept after taking the request back.');
  await expect(log.getByRole('button', { name: 'Earlier explanations' })).toHaveCount(0);
  await log.getByText('Kept after taking the request back.', { exact: true }).scrollIntoViewIfNeeded();
  await expect(log.getByText('Kept after taking the request back.', { exact: true })).toBeInViewport({ ratio: 1 });
  await writeFile(test.info().outputPath(`editor-log-${zoom * 100}.png`), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
  await page.getByRole('gridcell', { name: /^B · Documentation/ }).click();
  await page.locator('.editor-log > summary').click(); await page.clock.runFor(100);
  await expect(page.getByRole('region', { name: 'Editor log for Worker B' })).toContainText('No Editor explanations yet.');
});

test('status updates do not cancel an in-flight page of earlier explanations', async () => {
  await app.evaluate(({ ipcMain }) => {
    const fixture = (globalThis as any).editorLogFixture;
    fixture.logs['A/null'] = { entries: [], before: 50 };
    ipcMain.removeHandler('intercat:editor-log');
    ipcMain.handle('intercat:editor-log', (_event, query) => query.before === null ? fixture.logs['A/null'] : new Promise(resolve => {
      fixture.finishPage = () => resolve({ entries: [{ id: 49, trackId: 'A', scene: 'interpretation', inputId: null, title: 'Earlier record', line: '', detail: 'Retained across status updates.', createdAt: Date.now(), expiresAt: 0 }], before: null });
    }));
  });
  await page.locator('.editor-log > summary').click();
  const log = page.getByRole('region', { name: 'Editor log for Worker A' });
  await log.getByRole('button', { name: 'Earlier explanations' }).click();
  await page.clock.runFor(2200);
  await expect(log.getByRole('button', { name: 'Loading earlier…' })).toBeDisabled();
  await app.evaluate(() => (globalThis as any).editorLogFixture.finishPage());
  await expect(log).toContainText('Retained across status updates.');
  await expect(log.getByRole('button', { name: 'Loading earlier…' })).toHaveCount(0);
});

test('a fallback uses its displayed length for expiry and does not reappear', async () => {
  await update([{ ...entry(1, 'fork'), line: 'Too long '.repeat(20) }]);
  await expect(line()).toContainText('New track created.');
  await page.clock.runFor(4000);
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await update([{ ...entry(1, 'fork'), line: 'Too long '.repeat(20) }]);
  await expect(line()).toHaveAttribute('data-speaking', 'false');
});

test('closing details after the speech expires returns focus to the composer', async () => {
  await update([entry(1, 'fork')]);
  await line().getByRole('button', { name: 'more ›' }).click();
  await page.clock.runFor(12_000);
  await expect(page.getByRole('dialog', { name: 'Editor explanation details' })).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(line()).toHaveAttribute('data-speaking', 'false');
  await expect(page.getByRole('textbox', { name: 'Request draft', exact: true })).toBeFocused();
});

test('privacy, assumption and reply scenes retain their full detail when the line is too long', async () => {
  for(const [scene,fallback] of [['masked','Private details masked.'],['assumed','An assumption was recorded.'],['assumption-change','An assumption correction was requested.'],['reply','Reply draft opened.']]) {
    const current=await page.evaluate(()=>Date.now());
    const message={...entry(current,scene),line:'長いダミーの台詞'.repeat(30),createdAt:current,expiresAt:current+12_000,detail:`Complete dummy detail for ${scene}.`};
    await update([message]);
    await expect(line()).toContainText(fallback);
    await line().getByRole('button',{name:'more ›'}).click();
    await expect(page.getByRole('dialog',{name:'Editor explanation details'})).toContainText(message.detail);
    await page.keyboard.press('Escape');
  }
});
