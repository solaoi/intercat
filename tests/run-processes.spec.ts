import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const purpose of ['fix', 'merge'] as const) for (const zoom of [1, 2]) test(`a saved ${purpose} Run keeps its process controls after a lost reply at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-run-processes-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, { zoom, purpose }) => {
      const id = '01a1a200-0000-7000-8000-000000000001';
      const nonce = '01a1a200-0000-7000-8000-000000000002';
      const run = { id, owner:{kind:'commit',id:'old-dummy-review',revision:1}, revision: 8, track: 'dummy-a', purpose: { kind: purpose === 'merge' ? 'merge' : 'check' }, phase: 'done', approvals: [], prompts: [], output: null, failure: null, session: 'ended-dummy-session', turn: null, secretInput: null, refresh: null, restoring: null,
        remaining: { nonce, label: 'A', sessionName: 'Earlier dummy session', createdAt: Date.now(), revision: 0, state: 'waiting', error: null } as any };
      const state = { connected: true, sequence: 1, workerConfigured: true, tracks: [{ id: 'dummy-a', label: 'A', title: 'Dummy work', status: 'done' }], workers: [], utterances: [], repairRuns: [run], editor: { phase: 'ready', accepting: true }, privacy: { mode: 'basic', revision: 0 } };
      const fixture = { state, calls: [] as any[], lose: true };
      Object.assign(globalThis, { runProcessFixture: fixture });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
      ipcMain.removeHandler('intercat:repair-processes'); ipcMain.handle('intercat:repair-processes', (_event, request) => {
        fixture.calls.push(structuredClone(request));
        if (request.id !== id || request.nonce !== nonce) throw new Error('Wrong process owner');
        if (request.action === 'keep') {
          if (fixture.lose) { fixture.lose = false; throw new Error('Dummy lost acknowledgement'); }
          run.remaining = { ...run.remaining, state: 'kept', revision: 1 };
        } else if (request.action === 'stop') { run.remaining = { ...run.remaining, state: 'cleaning', revision: 3, error: 'Dummy cleanup could not finish.' }; } else { run.remaining = null; }
        state.sequence++; return { accepted: true };
      });
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
    }, { zoom, purpose });
    const page = await app.firstWindow(); await page.reload();
    const card = page.locator('[data-attention-card="run-processes-01a1a200-0000-7000-8000-000000000001"]');
    await expect(card).toBeAttached();
    if (!await card.isVisible()) await page.getByRole('button', { name: /^\+\d+ more/ }).click();
    await expect(card).toContainText('Worker A still has running processes');
    await expect(card).toContainText(`Left by A’s ${purpose} run`);
    await expect(card).toContainText('Earlier dummy session');
    await card.getByRole('button', { name: 'Keep', exact: true }).click();
    await card.getByRole('button', { name: 'Retry Keep', exact: true }).click();
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus(); await page.mouse.move(10, 10);
    await expect(card).toContainText('Kept running.');
    await expect(card.getByRole('button', { name: 'Keep', exact: true })).toBeDisabled();
    const stop = card.getByRole('button', { name: 'Stop it', exact: true });
    await stop.scrollIntoViewIfNeeded(); await expect(stop).toBeInViewport();
    const alignment = await card.locator('.run-process-actions').evaluate(element => ({ justify: getComputedStyle(element).justifyContent, gap: parseFloat(getComputedStyle(element).gap) }));
    expect(alignment.justify).toBe('flex-end'); expect(alignment.gap).toBeGreaterThanOrEqual(8);
    await stop.click();
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus(); await page.mouse.move(10, 10);
    await expect(card).toContainText('Worker A’s run needs attention');
    const cleanup = card.getByRole('button', { name: 'Retry cleanup', exact: true });
    await cleanup.scrollIntoViewIfNeeded(); await expect(cleanup).toBeInViewport();
    await cleanup.click();
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus(); await page.mouse.move(10, 10);
    await expect(card).toHaveCount(0);
    const calls = await app.evaluate(() => (globalThis as any).runProcessFixture.calls);
    expect(calls).toHaveLength(4); expect(calls[0]).toEqual(calls[1]);
    expect(calls[0]).toMatchObject({ action: 'keep', revision: 0 });
    expect(calls[2]).toMatchObject({ action: 'stop', revision: 1, nonce: calls[0].nonce, id: calls[0].id });
    expect(calls[3]).toMatchObject({ action: 'retry-cleanup', revision: 3, nonce: calls[0].nonce, id: calls[0].id });
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});
