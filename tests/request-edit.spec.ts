import { mockDrafts } from './support/drafts';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import type { DaemonStatus } from '../src/bridge';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-request-edit-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const word = { id: 'request', text: '説明を追加してください。'.repeat(60), attachments: [], state: 'placed', createdAt: Date.now(), revision: 1,
      target: { mode: 'none' }, decision: { kind: 'extend', trackId: 'A', boundary: 'after-turn' }, reason: null, editHold: null as null | { owner: string; token: string; revision: number } };
    const state = { connected: true, sequence: 1, workerConfigured: true, utterances: [word],
      tracks: [{ id: 'A', label: 'A', title: '元の作業', status: 'waiting' }, { id: 'B', label: 'B', title: '別の作業', status: 'working' }],
      reportQuestions: [{ id: 'question-a', reportId: 'report-a', trackId: 'A', sessionId: 'session-a', turnId: 'turn-a', revision: 1, kind: 'question', text: 'How should A continue?', state: 'open', replyUtteranceId: null }],
      workers: [{ trackId: 'B', phase: 'working', activeTurn: 'turn-b', sessionId: 'session-b', approvals: [], prompts: [], checkpointPending: false }] };
    const fixture = { state, calls: [] as Record<string, unknown>[], reject: false, loseNext: false, loseEnd: false, receipts: new Set<string>() };
    Object.assign(globalThis, { editFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:start-held'); ipcMain.handle('intercat:start-held', (_event, request) => {
      fixture.calls.push({ kind: 'start', ...request }); word.decision.boundary = 'now'; word.revision++; state.sequence++; return word;
    });
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => {
      fixture.calls.push({ kind: 'stage', ...request });
      return { ...word, id: request.opId, text: request.text, state: 'staged', editHold: null };
    });
    ipcMain.removeHandler('intercat:edit-request'); ipcMain.handle('intercat:edit-request', (_event, action) => {
      fixture.calls.push(action);
      if (action.kind === 'begin') {
        if (word.state === 'sent') throw new Error('This request was already sent.');
        if (word.editHold) throw new Error('This request is being edited in another window.');
        word.revision++; word.editHold = { owner: 'window-1', token: action.token, revision: word.revision };
      }
      if (action.kind === 'renew' && word.editHold?.token !== action.token) throw new Error('Editing was interrupted.');
      if (action.kind === 'apply') {
        if (fixture.reject) throw new Error('A newer edit owns this request. Reopen it.');
        if (fixture.receipts.has(action.opId)) return word;
        const lease = word.editHold;
        if (!lease || lease.revision !== action.revision) throw new Error('The edit version changed');
        if (action.edit.kind === 'text') word.text = action.edit.text;
        if (action.edit.kind === 'readAs') {
          word.target = { mode: 'lock', trackId: action.edit.trackId } as typeof word.target;
          word.decision = { kind: 'schedule', trackId: action.edit.trackId, boundary: action.edit.boundary };
        }
        word.revision++; lease.revision = word.revision; fixture.receipts.add(action.opId);
        if (fixture.loseNext) { fixture.loseNext = false; throw new Error('lost acknowledgement'); }
      }
      if (action.kind === 'end') {
        const receipt = 'end:' + action.token + ':' + JSON.stringify(action.edit);
        if (fixture.receipts.has(receipt)) return word;
        if (fixture.reject) throw new Error('A newer edit owns this request. Reopen it.');
        if (!word.editHold || word.editHold.token !== action.token || word.editHold.revision !== action.revision) throw new Error('The edit version changed');
        if (action.edit.kind === 'takeBack') { word.state = 'cut'; word.revision++; }
        if (action.edit.kind === 'text') { word.text = action.edit.text; word.state = 'staged'; word.revision++; }
        word.editHold = null; fixture.receipts.add(receipt);
        if (fixture.loseEnd) { fixture.loseEnd = false; throw new Error('lost finish acknowledgement'); }
      }
      state.sequence++; return word;
    });
  });
});
test.afterEach(async () => { if (app) await closeWindow(app); await rm(data, { recursive: true, force: true }); });

async function calls() {
  return app.evaluate(() => (globalThis as typeof globalThis & { editFixture: { calls: Record<string, unknown>[] } }).editFixture.calls);
}

test('three-section popover applies Read as immediately and keeps its lease until closed', async () => {
  const page = await app.firstWindow();
  await expect(page.locator('.request-select')).toBeVisible();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(popup).toHaveAttribute('aria-modal', 'false');
  await expect(popup.getByRole('heading', { name: 'You said' })).toBeVisible();
  await expect(popup.getByRole('heading', { name: 'Read as' })).toBeVisible();
  await expect(popup.getByText('Decision timing', { exact: true })).toHaveCount(0);
  const destination = popup.getByRole('combobox', { name: 'Edited request destination' });
  await expect(destination).toBeEnabled(); await destination.selectOption('B');
  await expect(destination).toHaveValue('B');
  await expect(popup.getByLabel('Stop B and send')).toBeVisible();
  await expect(popup.getByText('stops the current turn, then sends this (⌘.)', { exact: true })).toBeVisible();
  await popup.getByLabel('When I say').check();
  await expect(popup.getByLabel('When I say')).toBeChecked();
  await expect(popup.getByRole('button', { name: /Save changes/ })).toHaveCount(0);
  const takeBack = popup.getByRole('button', { name: 'Take back', exact: true });
  await expect(takeBack).toBeEnabled(); await takeBack.focus(); await expect(takeBack).toBeInViewport();
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('read-as-200.png'), Buffer.from(pixels, 'base64'));
  expect((await calls()).filter(call => call.kind === 'apply')).toHaveLength(2);
  expect((await calls()).some(call => call.kind === 'end')).toBe(false);
  await popup.getByRole('button', { name: 'Close request details' }).click();
  await expect(popup).not.toBeVisible();
  await expect(page.locator('.request-select')).toContainText('waiting for your go');
  await page.getByRole('button', { name: 'Start request:', exact: false }).click();
  await expect(page.locator('.request-select')).not.toContainText('waiting for your go');
});

test('trim labels distinguish an unsent stop, stopping, and the sent correction', async () => {
  const page = await app.firstWindow();
  const cases = [
    ['stopping', 'redirecting', false, 'redirect A · waiting to stop'],
    ['interrupting', 'redirecting', true, 'redirect A · sent · stopping'],
    ['ready', 'placed', true, 'redirect A · waiting to send'],
    ['sent', 'sent', true, 'redirect A · sent · waiting for A to pick it up'],
    ['held', 'held', true, 'redirect A · held'],
  ] as const;
  for (const [phase, state, interruptStarted, label] of cases) {
    await app.evaluate((_electron, input) => {
      const fixture = (globalThis as typeof globalThis & { editFixture: { state: DaemonStatus } }).editFixture;
      const word = fixture.state.utterances![0];
      word.decision!.kind = 'trim';
      word.decision!.boundary = 'now';
      word.state = input.state;
      word.interruptStarted = input.interruptStarted;
      fixture.state.redirects = [{ inputId: word.id, revision: word.revision, trackId: 'A', sessionId: 'session-a', turnId: 'turn-a', phase: input.phase }];
      fixture.state.sequence!++;
    }, { phase, state, interruptStarted });
    const status = page.locator('.request-select small').last();
    await expect(status).toHaveText(label);
    if (interruptStarted) await expect(page.getByRole('button', { name: /^Cut request:/ })).toHaveCount(0);
    if (phase === 'sent') {
      await app.evaluate(({ BrowserWindow }) => {
        const window = BrowserWindow.getAllWindows()[0];
        window.setSize(1040, 760);
        window.webContents.setZoomFactor(2);
      });
      await status.scrollIntoViewIfNeeded();
      await expect(status).toBeInViewport();
      expect(await page.locator('.request-strip').evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
      const strip = (await page.locator('.request-strip').boundingBox())!;
      const statusBox = (await status.boundingBox())!;
      expect(statusBox.y).toBeGreaterThanOrEqual(strip.y);
      expect(statusBox.y + statusBox.height).toBeLessThanOrEqual(strip.y + strip.height);
      expect(await status.evaluate(element => element.clientHeight / parseFloat(getComputedStyle(element).lineHeight))).toBeLessThanOrEqual(3);
      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath('trim-sent-200.png'), Buffer.from(pixels, 'base64'));
    }
  }
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { editFixture: { state: DaemonStatus } }).editFixture;
    fixture.state.utterances![0].state = 'landed';
    fixture.state.redirects![0].phase = 'done';
    fixture.state.sequence!++;
  });
  await expect(page.locator('.request-select')).toHaveCount(0);
});

test('decision timings keep both requests and remain readable at 200 percent', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => {
    const fixture = globalThis as typeof globalThis & { editFixture: { state: { utterances: { decision: object }[] } } };
    Object.assign(fixture.editFixture.state.utterances[0].decision, {
      inferenceMs: 60, timedOut: false,
      timings: [{ roundTripMs: 1450, inferenceMs: 50 }, { roundTripMs: 900, inferenceMs: 60 }],
    });
  });
  await page.reload();
  await app.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    window.setSize(1040, 760);
    window.webContents.setZoomFactor(2);
  });
  await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await popup.getByText('Decision timing', { exact: true }).click();
  const first = popup.locator('dl[aria-label="Decision request 1"] dd');
  const second = popup.locator('dl[aria-label="Decision request 2"] dd');
  await expect(first.nth(0)).toHaveText('1,450 ms');
  await expect(first.nth(1)).toHaveText('50 ms');
  await expect(second.nth(0)).toHaveText('900 ms');
  await expect(second.nth(1)).toHaveText('60 ms');
  await second.nth(1).scrollIntoViewIfNeeded();
  await expect(second.nth(1)).toBeInViewport();
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('decision-timing-200.png'), Buffer.from(pixels, 'base64'));
  const takeBack = popup.getByRole('button', { name: 'Take back', exact: true });
  await takeBack.focus();
  await expect(takeBack).toBeInViewport();
});

test('You said moves to the composer and IME Escape keeps its edit lease', async () => {
  const page = await app.firstWindow();
  await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await popup.getByRole('button', { name: 'Edit text', exact: true }).click();
  const text = page.getByRole('textbox', { name: 'Request draft' });
  await expect(popup).not.toBeVisible();
  await expect(page.getByLabel('Request being edited')).toBeVisible();
  await text.fill('未確定の編集');
  await expect(page.locator('.request-select small').last()).toHaveText('editing');
  await text.dispatchEvent('keydown', { key: 'Escape', isComposing: true, bubbles: true });
  await expect(page.getByLabel('Request being edited')).toBeVisible();
  expect((await calls()).filter(call => call.kind === 'end')).toHaveLength(0);
  await page.keyboard.press('Escape');
  await expect(page.getByLabel('Request being edited')).toHaveCount(0);
  await expect(page.locator('.request-select')).toContainText('説明を追加してください。');
  await page.locator('.request-select').click();
  await popup.getByRole('button', { name: 'Edit text', exact: true }).click();
  await text.fill('更新した依頼'); await text.press('Meta+Enter');
  await expect(page.getByLabel('Request being edited')).toHaveCount(0);
  await expect(page.locator('.request-select')).toContainText('更新した依頼');
});

test('heartbeat and a lost acknowledgement preserve the same immediate change', async () => {
  const page = await app.firstWindow(); await page.clock.install();
  await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(popup.getByRole('combobox')).toBeEnabled(); await page.clock.runFor(10_100);
  expect((await calls()).some(call => call.kind === 'renew')).toBe(true);
  await page.clock.resume();
  await app.evaluate(() => { (globalThis as typeof globalThis & { editFixture: { loseNext: boolean } }).editFixture.loseNext = true; });
  await popup.getByRole('combobox').selectOption('B');
  await popup.getByRole('button', { name: 'Retry change' }).click();
  await expect(popup.getByRole('combobox')).toBeEnabled();
  expect(await popup.evaluate(element => element.contains(document.activeElement))).toBe(true);
  const attempts = (await calls()).filter(call => call.kind === 'apply');
  expect(attempts).toHaveLength(2); expect(attempts[0]).toEqual(attempts[1]);
  await page.keyboard.press('Escape'); await expect(popup).not.toBeVisible();
});

test('a replaced lease cannot save and its popover can still close with Escape', async () => {
  const page = await app.firstWindow(); await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(popup.getByRole('combobox')).toBeEnabled();
  await app.evaluate(() => { (globalThis as typeof globalThis & { editFixture: { reject: boolean } }).editFixture.reject = true; });
  await popup.getByRole('combobox').selectOption('B');
  await expect(popup.getByRole('status')).toContainText('A newer edit owns');
  await page.keyboard.press('Escape'); await expect(popup).not.toBeVisible();
});

test('Take back closes the popover and removes the unsent request', async () => {
  const page = await app.firstWindow(); await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await popup.getByRole('button', { name: 'Take back', exact: true }).click();
  await expect(popup).not.toBeVisible(); await expect(page.locator('.request-select')).toHaveCount(0);
  expect((await calls()).some(call => call.kind === 'end' && (call.edit as { kind: string }).kind === 'takeBack')).toBe(true);
});

test('the popover keeps ordinary region navigation and closes when another control is used', async () => {
  const page = await app.firstWindow(); await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(popup.getByRole('combobox')).toBeEnabled();
  await page.keyboard.press('Control+Tab');
  await expect(page.getByRole('complementary', { name: 'Sessions' })).toBeFocused();
  await expect(popup).toBeVisible();
  await page.getByRole('button', { name: 'Settings', exact: true }).click();
  await expect(popup).not.toBeVisible();
  expect((await calls()).some(call => call.kind === 'end' && (call.edit as { kind: string }).kind === 'cancel')).toBe(true);
});


test('reopening a popover and writing a Reply keep the original composer edit held', async () => {
  const page = await app.firstWindow(); await page.clock.install();
  await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await popup.getByRole('button', { name: 'Edit text', exact: true }).click();
  const text = page.getByRole('textbox', { name: 'Request draft' });
  await text.fill('Keep this edited draft');
  await page.locator('.request-select').click();
  await popup.getByRole('button', { name: 'Close request details' }).click();
  await expect(text).toHaveValue('Keep this edited draft');
  expect((await calls()).filter(call => call.kind === 'end')).toHaveLength(0);
  await page.getByRole('group', { name: 'Question from Worker A' }).getByRole('button', { name: 'Reply', exact: true }).click();
  await expect(text).toHaveValue(''); await text.fill('A separate reply');
  await page.clock.runFor(10_100);
  expect((await calls()).filter(call => call.kind === 'renew').length).toBeGreaterThan(0);
  expect((await calls()).filter(call => call.kind === 'end')).toHaveLength(0);
  await page.clock.resume();
  await page.getByRole('button', { name: 'Cancel reply', exact: true }).click();
  await expect(text).toHaveValue('Keep this edited draft');
  await page.getByRole('button', { name: 'Update request', exact: false }).click();
  await expect(page.locator('.request-select')).toContainText('Keep this edited draft');
});

test('an already sent word requires an explicit choice before creating a new instruction', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { const state = (globalThis as typeof globalThis & { editFixture: { state: { utterances: { state: string }[]; sequence: number } } }).editFixture.state; state.utterances[0].state = 'sent'; state.sequence++; });
  await expect(page.locator('.request-select')).toContainText('sent');
  await page.locator('.request-select').click();
  await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
  await expect(page.getByLabel('Request being edited')).toContainText('Already sent');
  await page.getByRole('textbox', { name: 'Request draft' }).fill('A new instruction');
  expect((await calls()).filter(call => call.kind === 'stage')).toHaveLength(0);
  await page.getByRole('button', { name: 'Send as a new instruction', exact: true }).click();
  await expect(page.getByLabel('Request being edited')).toHaveCount(0);
  expect((await calls()).filter(call => call.kind === 'stage')).toHaveLength(1);
});

test('a lost edit acknowledgement replays its saved finish after reload', async () => {
  const page = await app.firstWindow();
  await page.locator('.request-select').click();
  await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('Retry the original edit');
  await app.evaluate(() => { (globalThis as typeof globalThis & { editFixture: { loseEnd: boolean } }).editFixture.loseEnd = true; });
  await page.getByRole('button', { name: 'Update request', exact: false }).click();
  await expect(page.getByRole('button', { name: 'Retry save', exact: false })).toBeVisible();
  await page.locator('.request-select').click();
  await expect(page.getByRole('dialog', { name: 'Request', exact: true })).toHaveCount(0);
  await page.reload();
  await page.getByRole('button', { name: 'Retry save', exact: false }).click();
  await expect(page.getByLabel('Request being edited')).toHaveCount(0);
  const finishes = (await calls()).filter(call => call.kind === 'end');
  expect(finishes).toHaveLength(2); expect(finishes[0]).toEqual(finishes[1]);
});

test('a restored composer reacquires an interrupted lease without losing its text', async () => {
  const page = await app.firstWindow();
  await page.locator('.request-select').click();
  await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
  const text = page.getByRole('textbox', { name: 'Request draft' });
  await text.fill('Continue this edit after restart');
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { editFixture: { state: { sequence: number; utterances: { state: string; editHold: unknown; revision: number }[] } } }).editFixture.state;
    const word = state.utterances[0]; word.state = 'held'; word.editHold = null; word.revision++; state.sequence++;
  });
  await page.reload();
  await expect(text).toHaveValue('Continue this edit after restart');
  await expect.poll(async () => (await calls()).filter(call => call.kind === 'begin').length).toBe(2);
  await page.getByRole('button', { name: 'Update request', exact: false }).click();
  await expect(page.locator('.request-select')).toContainText('Continue this edit after restart');
  const begins = (await calls()).filter(call => call.kind === 'begin');
  const finish = (await calls()).find(call => call.kind === 'end');
  expect(finish?.token).toBe(begins[1].token);
});

test('a restored draft cannot overwrite a different window’s current edit', async () => {
  const page = await app.firstWindow();
  await page.locator('.request-select').click();
  await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
  const text = page.getByRole('textbox', { name: 'Request draft' });
  await text.fill('Keep my local edit');
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { editFixture: { state: { sequence: number; utterances: { editHold: unknown; revision: number }[] } } }).editFixture.state;
    const word = state.utterances[0]; word.revision++;
    word.editHold = { owner: 'another-window', token: 'another-token', revision: word.revision }; state.sequence++;
  });
  await page.reload();
  await expect(page.getByLabel('Request being edited')).toContainText('another window');
  await expect(page.locator('.request-select small').last()).toHaveText('editing in another window');
  await page.getByRole('button', { name: 'Update request', exact: false }).click();
  await expect(text).toHaveValue('Keep my local edit');
  expect((await calls()).filter(call => call.kind === 'end')).toHaveLength(0);
  expect((await calls()).filter(call => call.kind === 'begin')).toHaveLength(1);
});

test('the Japanese edit context and composer actions fit a small window at 200 percent', async () => {
  const page = await app.firstWindow();
  await page.locator('.request-select').click();
  await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 720); window.webContents.setZoomFactor(2); });
  await expect(page.getByRole('textbox', { name: 'Request draft' })).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Cancel edit', exact: true })).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Update request', exact: false })).toBeInViewport();
  expect(await page.locator('.composer').evaluate(element => element.scrollWidth <= element.clientWidth + 1)).toBe(true);
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath('composer-200.png'), Buffer.from(pixels, 'base64'));
});

test('an obsolete Reply can be edited as an ordinary request', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { editFixture: { state: { sequence: number; utterances: { state: string }[] } } }).editFixture.state;
    state.utterances[0].state = 'orphaned'; state.sequence++;
  });
  await expect(page.locator('.orphaned-request')).toBeAttached();
  await page.getByRole('button', { name: /^\+\d+ more/ }).click();
  await page.getByRole('group', { name: 'Reply needs a destination' }).getByRole('button', { name: 'Edit', exact: true }).click();
  await expect(page.getByLabel('Request being edited')).toBeVisible();
  await page.getByRole('textbox', { name: 'Request draft' }).fill('An ordinary instruction');
  await page.getByRole('button', { name: 'Update request', exact: false }).click();
  await expect(page.getByRole('group', { name: 'Reply needs a destination' })).toHaveCount(0);
  await expect(page.locator('.request-select')).toContainText('An ordinary instruction');
});

test('a held request whose stop already started offers only a new instruction', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { editFixture: { state: { sequence: number; utterances: { state: string; interruptStarted?: boolean }[] } } }).editFixture.state;
    state.utterances[0].state = 'held'; state.utterances[0].interruptStarted = true; state.sequence++;
  });
  await expect(page.getByRole('button', { name: 'Cut request:', exact: false })).toHaveCount(0);
  await page.locator('.request-select').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(popup).toContainText('Stop already sent');
  await popup.getByRole('button', { name: 'Edit text', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Send as a new instruction', exact: true })).toBeVisible();
  expect((await calls()).filter(call => call.kind === 'begin')).toHaveLength(0);
});

test('a pasted request keeps its source when editing the one-line instruction and role', async () => {
  const page = await app.firstWindow();
  const id = '01960000-0000-7000-8000-000000000001';
  await app.evaluate((_electron, id) => {
    const fixture = (globalThis as typeof globalThis & { editFixture: { state: { utterances: object[]; sequence: number } } }).editFixture;
    Object.assign(fixture.state.utterances[0], { sourceText: '', sources: [{ attachmentId: id, role: 'request', after: '' }], attachments: [{ id, name: 'pasted.txt', byteLen: 900, sha256: 'a'.repeat(64) }] });
    fixture.state.sequence++;
  }, id);
  await page.locator('.request-select').click();
  await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
  const field = page.getByRole('textbox', { name: 'Request draft', exact: true });
  await expect(field).toHaveValue('');
  await expect(page.getByRole('button', { name: 'Attach as reference', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: 'Attach as reference', exact: true }).click();
  await field.fill('Explain the pasted material.');
  await page.getByRole('textbox', { name: 'Text after pasted block 1' }).fill('Keep all files unchanged.');
  await page.getByRole('button', { name: /^Update request/ }).click();
  await expect(page.getByLabel('Request being edited')).toHaveCount(0);
  const finished = (await calls()).filter(call => call.kind === 'end');
  expect(finished).toHaveLength(1);
  expect(finished[0]).toMatchObject({ edit: { kind: 'text', text: 'Explain the pasted material.', sources: [{ attachmentId: id, role: 'reference', after: 'Keep all files unchanged.' }] } });
});

test('completed go-ahead details show the application result without requesting a worker turn', async () => {
  await app.evaluate(({ ipcMain }) => {
    const fixture = (globalThis as typeof globalThis & { editFixture: { state: DaemonStatus } }).editFixture;
    const word = fixture.state.utterances![0];
    word.text = 'Start waiting work';
    word.state = 'landed';
    word.decision = { kind: 'command', command: 'start-waiting', trackId: 'A', boundary: null, confidence: 1, inferenceMs: 1, timedOut: false };
    word.startWaiting = { candidates: [{ inputId: 'waiting', revision: 1, text: '待機していた依頼', label: 'A', available: false }], started: { inputId: 'waiting', revision: 2 } };
    fixture.state.sequence = (fixture.state.sequence ?? 0) + 1;
    const now = Date.now();
    fixture.state.timeline = { clock: { epochMs: now, monoMs: 0, timelineMs: now, bootId: 'fixture' }, summaries: [] };
    ipcMain.removeHandler('intercat:timeline-window');
    ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [{
      id: 'input/request', inputId: word.id, trackId: 'A', repairRunId: null,
      at: now - 1000, text: word.text, operation: 'command', command: 'start-waiting', state: 'landed',
    }], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:utterance-read');
    ipcMain.handle('intercat:utterance-read', () => word);
  });
  const page = await app.firstWindow();
  await page.reload();
  await page.getByLabel('Timeline requests', { exact: true }).getByRole('button').click();
  const popup = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(popup).toContainText('Application command · start-waiting');
  await expect(popup).toContainText('Start requested · Worker A');
  await expect(popup).toContainText('待機していた依頼');
  await expect(popup.getByRole('status')).toHaveText('Application command completed.');
  await expect(popup.locator('.request-turn')).toHaveCount(0);
});
