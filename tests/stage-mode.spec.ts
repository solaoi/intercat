import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, Utterance, WorkerState } from '../src/bridge';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

for (const zoom of [1, 2]) test(`stage mode keeps the draft and opens its cards above the badge at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-stage-mode-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      const request = (id: string): Utterance => ({ id, text: `Choose a destination for ${id}`, attachments: [], state: 'asked', revision: 1,
        createdAt: 1, target: { mode: 'none' }, decision: null, reason: 'Where should this request go?' });
      const worker: WorkerState = { trackId: 'A', phase: 'ready', sessionId: 'session-A', activeTurn: null, identity: null, detail: null,
        checkpointPending: false, checkpoint: null, approvals: [], prompts: [], output: 'A completed test report. '.repeat(50) };
      const state: DaemonStatus = { connected: true, sequence: 1, workerConfigured: true,
        session: { id: 'stage', name: 'Stage layout', slug: 'stage', createdAt: 1 },
        tracks: [{ id: 'A', label: 'A', title: 'Arithmetic', status: 'ready' }, { id: 'B', label: 'B', title: 'Other work', status: 'ready' }],
        editorEntries: [{ id: 1, trackId: 'A', inputId: 'idea', scene: 'fork', title: 'An idea', line: 'New track for A.', detail: 'The idea remains available.', createdAt: Date.now(), expiresAt: Date.now() + 120_000 }], workers: [worker], utterances: [request('first'), request('second'), { ...request('idea'), state: 'placed',
          decision: { kind: 'fork', trackId: 'A', boundary: null, command: null, confidence: 1, inferenceMs: 1, timedOut: false,
            description: { title: 'An idea', line: 'New track for A.', detail: 'The idea remains available.', createdAt: Date.now() + 60_000 } } }] };
      Object.assign(globalThis, { stageFixture: { state } });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: state.sequence, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, value) => {
        const word = state.utterances!.find(word => word.id === value.utteranceId)!;
        word.state = 'held'; word.revision++; state.sequence!++;
        return word;
      });
    });
    const page = await app.firstWindow();
    await page.reload();
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(zoom); }, zoom);
    const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await draft.fill('この下書きと宛先を保つ');
    const destination = page.getByRole('combobox', { name: 'Request destination', exact: true });
    await destination.selectOption('B');
    await draft.focus();
    await page.keyboard.press('Meta+Shift+T');
    await expect(page.locator('.app')).toHaveClass(/stage-mode/);
    await expect(draft).toBeInViewport({ ratio: 1 });
    await expect(draft).toHaveValue('この下書きと宛先を保つ');
    await expect(destination).toHaveValue('B');
    await expect(page.getByRole('region', { name: 'Editor explanation', exact: true })).toBeVisible();
    await expect(page.locator('.stage-workers')).toBeHidden();
    const badge = page.getByRole('button', { name: 'Needs you · 2', exact: true });
    await expect(badge).toBeInViewport();
    await expect(page.getByRole('button', { name: 'Commit', exact: true })).toBeInViewport();
    const before = (await page.locator('.stage').boundingBox())!;
    await writeFile(test.info().outputPath('layout.json'), JSON.stringify(await page.evaluate(() => ({ stage: document.querySelector('.stage')?.getBoundingClientRect().toJSON(), desk: document.querySelector('.desk')?.getBoundingClientRect().toJSON(), elements: [...document.querySelectorAll('.desk > *, .desk-scroll > *')].map(element => ({ class: element.className, rect: element.getBoundingClientRect().toJSON(), display: getComputedStyle(element).display })) })), null, 2));
    const layoutPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`layout-${zoom * 100}.png`), Buffer.from(layoutPixels, 'base64'));
    expect(before.height).toBeGreaterThan(112);
    await badge.click();
    const cards = page.getByRole('dialog', { name: 'Needs you cards', exact: true });
    await expect(cards).toBeVisible();
    await expect(cards.locator('[data-attention-card]:visible')).toHaveCount(2);
    const popup = (await cards.boundingBox())!;
    const badgeBox = (await badge.boundingBox())!;
    expect(popup.y + popup.height).toBeLessThanOrEqual(badgeBox.y);
    expect((await page.locator('.stage').boundingBox())!.height).toBe(before.height);
    const popupPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`cards-${zoom * 100}.png`), Buffer.from(popupPixels, 'base64'));
    await cards.locator('[data-attention-card]').first().getByRole('button', { name: 'Hold', exact: true }).click();
    await expect(badge).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Needs you · 1', exact: true })).toBeVisible();
    await page.keyboard.press('Escape');
    await expect(cards).not.toBeVisible();
    await draft.focus();
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath(`stage-mode-${zoom * 100}.png`), Buffer.from(pixels, 'base64'));
    await page.keyboard.press('Meta+Shift+T');
    await expect(page.locator('.app')).not.toHaveClass(/stage-mode/);
    await expect(page.locator('.stage-workers')).toBeVisible();
    await expect(draft).toHaveValue('この下書きと宛先を保つ');
    await expect(destination).toHaveValue('B');
    if (zoom === 1) {
      await page.getByRole('button', { name: 'Settings', exact: true }).click();
      await page.keyboard.press('Meta+Shift+T');
      await expect(page.locator('.app')).not.toHaveClass(/stage-mode/);
      await page.getByRole('button', { name: 'Close settings', exact: true }).click();
      const divider = page.getByRole('separator', { name: 'Resize stage and desk', exact: true });
      const box = (await divider.boundingBox())!;
      const maximum = Number(await divider.getAttribute('aria-valuemax'));
      const height = Number(await divider.getAttribute('aria-valuenow'));
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.down();
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2 + maximum - 40 - height, { steps: 5 });
      await page.mouse.up();
      await expect(divider).toHaveAttribute('aria-valuetext', 'Transcript collapsed');
      await expect(page.locator('.stage-workers')).toBeHidden();
      await expect(page.locator('.stage-cards-inline')).toBeVisible();
      await expect(draft).toBeInViewport({ ratio: 1 });
      await divider.focus();
      await page.keyboard.press('End');
      await expect(page.locator('.app')).toHaveClass(/stage-mode/);
      await divider.dblclick();
      await expect(page.locator('.app')).not.toHaveClass(/stage-mode|desk-layout-adjusted/);
      await expect(page.locator('.stage-workers')).toBeVisible();
      await app.evaluate(() => {
        const state = (globalThis as typeof globalThis & { stageFixture: { state: DaemonStatus } }).stageFixture.state;
        state.workers = []; state.utterances = []; state.sequence!++;
      });
      await page.mouse.move(5, 5);
      await draft.focus();
      await expect(page.getByRole('region', { name: 'Needs you', exact: true })).toHaveCount(0);
      const largeStage = (await page.locator('.stage').boundingBox())!.height;
      await divider.focus();
      await page.keyboard.press('ArrowUp');
      await expect(divider).toHaveAttribute('aria-valuetext', /pixels$/);
      expect((await page.locator('.stage').boundingBox())!.height).toBeLessThan(largeStage);
    }
    await draft.focus();
    await page.keyboard.press('Meta+Shift+T');
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { stageFixture: { state: DaemonStatus } }).stageFixture.state;
      state.workers = []; state.utterances = []; state.sequence!++;
    });
    await expect(page.getByRole('button', { name: 'Needs you · 0', exact: true })).toBeDisabled();
    await expect(draft).toBeInViewport({ ratio: 1 });
    await app.evaluate(() => {
      const state = (globalThis as typeof globalThis & { stageFixture: { state: DaemonStatus } }).stageFixture.state;
      state.reportQuestions = [{ id: 'report-question', reportId: 'report', trackId: 'A', sessionId: 'session-A', turnId: 'turn', revision: 1, kind: 'question', text: 'Choose the next step.', state: 'open', replyUtteranceId: null }];
      state.sequence!++;
    });
    const reportBadge = page.getByRole('button', { name: 'Needs you · 1', exact: true });
    await expect(reportBadge).toBeInViewport();
    await expect(draft).toBeInViewport({ ratio: 1 });
    expect(await page.locator('.main').evaluate(element => element.scrollHeight - element.clientHeight)).toBeLessThanOrEqual(1);
    await reportBadge.click();
    await expect(page.getByRole('dialog', { name: 'Needs you cards', exact: true })).toContainText('Choose the next step.');
    await page.keyboard.press('Escape');
    await expect(draft).toBeInViewport({ ratio: 1 });
  } finally {
    await closeWindow(app);
    await rm(directory, { recursive: true, force: true });
  }
});
