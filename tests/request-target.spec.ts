import { mockDrafts } from './support/drafts';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';

let app: ElectronApplication;
let data: string;
test.beforeEach(async () => {
  data = await mkdtemp('/private/tmp/ic-request-target-');
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const state = { connected: true, sequence: 1, workerConfigured: true,
      tracks: [{ id: 'A', label: 'A', title: 'APIを直す', status: 'waiting' },
        { id: 'B', label: 'B', title: '説明を日本語で書き直す', status: 'waiting' }],
      workers: [], utterances: [] as unknown[] };
    const calls: unknown[] = [];
    Object.assign(globalThis, { targetFixture: { state, calls, loseFirst: false } });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:stage'); ipcMain.handle('intercat:stage', (_event, request) => {
      calls.push(request);
      if ((globalThis as typeof globalThis & { targetFixture: { loseFirst: boolean } }).targetFixture.loseFirst && calls.length === 1) throw new Error('lost acknowledgement');
      const word = { id: request.opId, text: request.text, target: request.target, attachments: [],
        state: 'staged', createdAt: Date.now(), revision: 1, decision: null, reason: null };
      state.utterances = [word]; state.sequence++;
      return word;
    });
  });
});
test.afterEach(async () => { if (app) await closeWindow(app); await rm(data, { recursive: true, force: true }); });

test('a locked draft stays addressed to B while browsing A and retrying its save', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => { (globalThis as typeof globalThis & { targetFixture: { loseFirst: boolean } }).targetFixture.loseFirst = true; });
  const target = page.getByRole('combobox', { name: 'Request destination' });
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await expect(target).toBeEnabled();
  await page.locator('#track-B').click();
  await draft.fill('この説明に注意点を足してください');
  await target.selectOption('B');
  await page.locator('#track-A').click();
  await expect(target).toHaveValue('B');
  await mkdir('research/verify/m2-target-20261105', { recursive: true });
  const lockedPixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/m2-target-20261105/locked-b-100.png', Buffer.from(lockedPixels, 'base64'));
  await page.getByRole('button', { name: 'Put', exact: false }).click();
  await expect(page.getByRole('button', { name: 'Retry save', exact: false })).toBeVisible();
  await expect(target).toBeDisabled();
  await expect(page.getByRole('button', { name: 'New track', exact: true })).toBeDisabled();
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { tracks: { id: string }[]; sequence: number } } }).targetFixture;
    fixture.state.tracks = fixture.state.tracks.filter(track => track.id !== 'B'); fixture.state.sequence++;
  });
  await expect(target.locator('option:checked')).toHaveText('Unavailable track');
  await page.getByRole('button', { name: 'Retry save', exact: false }).click();
  await expect(draft).toHaveValue('');
  await expect(target).toHaveValue('');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls);
  expect(calls).toHaveLength(2);
  expect(calls[0]).toEqual(calls[1]);
  expect(calls[0]).toMatchObject({ target: { mode: 'lock', trackId: 'B' }, selectedTrackId: 'A' });
});

for (const zoom of [1, 2]) test(`the whole timeline row highlights and opens its worker without changing the draft at ${zoom * 100}%`, async () => {
  await app.evaluate(({ BrowserWindow }) => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { sequence: number } } }).targetFixture;
    Object.assign(fixture.state, { workers: ['A', 'B'].map(id => ({ trackId: id, phase: 'ready', sessionId: `dummy-${id}`,
      activeTurn: null, identity: null, detail: null, checkpointPending: false, checkpoint: null, approvals: [], prompts: [],
      output: `${id}の結果です。保存した内容を確認してください。`.repeat(12) })) });
    fixture.state.sequence++;
    BrowserWindow.getAllWindows()[0].setSize(1280, 1000);
  });
  const page = await app.firstWindow();
  await page.reload();
  await app.evaluate(({ BrowserWindow }, zoom) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom), zoom);
  const draft = page.getByRole('textbox', { name: 'Request draft', exact: true });
  const destination = page.getByRole('combobox', { name: 'Request destination', exact: true });
  await draft.fill('Aに追加する依頼を保つ'); await destination.selectOption('A');
  if (zoom === 2) {
    await page.keyboard.press('Meta+Shift+T');
    await expect(page.locator('.app')).toHaveClass(/stage-mode/);
  }
  const row = page.locator('#track-B');
  await row.scrollIntoViewIfNeeded();
  const lane = (await row.boundingBox())!;
  const timeline = page.getByRole('region', { name: 'Timeline', exact: true });
  const bounds = (await timeline.boundingBox())!;
  const point = { x: bounds.x + bounds.width - 24, y: Math.min(lane.y + lane.height, bounds.y + bounds.height) - 8 };
  expect(point.y).toBeGreaterThanOrEqual(lane.y);
  await writeFile(test.info().outputPath(`row-layout-${zoom}.json`), JSON.stringify(await page.evaluate(point => ({ point,
    hit: document.elementFromPoint(point.x, point.y)?.outerHTML,
    viewport: { width: innerWidth, height: innerHeight },
    layout: [...document.querySelectorAll('.stage,.timeline,.timeline-scroll,#track-B')].map(element => ({ class: element.className,
      rect: element.getBoundingClientRect().toJSON(), scrollTop: element.scrollTop })) }), point), null, 2));
  await writeFile(test.info().outputPath(`before-row-${zoom}.png`), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
  await page.mouse.move(point.x, point.y);
  await expect(timeline).toHaveAttribute('data-hovered-row', 'track-B');
  await expect(row).toHaveAttribute('aria-selected', 'false');
  expect(await page.evaluate(point => getComputedStyle(document.elementFromPoint(point.x, point.y)!).cursor, point)).toBe('pointer');
  await writeFile(test.info().outputPath(`row-hover-${zoom}.png`), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
  await page.mouse.click(point.x, point.y);
  await expect(row).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('.app')).not.toHaveClass(/stage-mode/);
  await expect(page.locator('.desk-selection')).toHaveText('説明を日本語で書き直す');
  await expect(page.getByRole('log', { name: 'Worker B result', exact: true })).toBeInViewport();
  await writeFile(test.info().outputPath(`row-open-${zoom}.png`), Buffer.from(await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')), 'base64'));
  await expect(draft).toHaveValue('Aに追加する依頼を保つ');
  await expect(destination).toHaveValue('A');
  await page.mouse.move(0, 0);
  await expect(timeline).not.toHaveAttribute('data-hovered-row');
  await page.getByRole('grid', { name: 'Tracks', exact: true }).focus();
  await page.keyboard.press('Home'); await page.keyboard.press('Enter');
  await expect(page.getByRole('log', { name: 'Worker A result', exact: true })).toBeInViewport();
  await page.keyboard.press('ArrowDown'); await page.keyboard.press('Enter');
  await expect(page.getByRole('log', { name: 'Worker B result', exact: true })).toBeInViewport();
  await expect(destination).toHaveValue('A');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls)).toEqual([]);
});

test('clearing the draft releases its target and a missing target cannot silently receive work', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  const target = page.getByRole('combobox', { name: 'Request destination' });
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('Bへの依頼'); await target.selectOption('B');
  await draft.fill(''); await expect(target).toHaveValue('');
  await draft.fill('再びBへの依頼'); await target.selectOption('B');
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { tracks: { id: string }[]; sequence: number } } }).targetFixture;
    fixture.state.tracks = fixture.state.tracks.filter(track => track.id !== 'B'); fixture.state.sequence++;
  });
  await expect(target.locator('option:checked')).toHaveText('Unavailable track');
  await expect(page.getByRole('button', { name: 'Put', exact: false })).toBeDisabled();
  await target.selectOption('');
  await expect(page.getByRole('button', { name: 'Put', exact: false })).toBeEnabled();
  await expect(target).toBeInViewport();
  await expect(page.getByRole('button', { name: 'Put', exact: false })).toBeInViewport();
  await mkdir('research/verify/m2-target-20261105', { recursive: true });
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/m2-target-20261105/target-200.png', Buffer.from(pixels, 'base64'));
});

test('Say to B changes only the draft destination and its menu stays usable at 200 percent', async () => {
  const page = await app.firstWindow();
  const draft = page.getByRole('textbox', { name: 'Request draft' });
  await draft.fill('Bへ送る前に、Aの結果を確認します');
  await page.locator('#track-A').click();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  const actions = page.getByRole('button', { name: 'Actions for Worker B' });
  await actions.focus();
  await page.keyboard.press('Enter');
  const say = page.getByRole('menuitem', { name: 'Say to B', exact: true });
  await expect(say).toBeInViewport();
  await say.click();
  await expect(draft).toBeFocused();
  await expect(draft).toHaveValue('Bへ送る前に、Aの結果を確認します');
  await expect(page.getByRole('combobox', { name: 'Request destination' })).toHaveValue('B');
  await expect(page.locator('#track-A')).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#track-B')).toHaveAttribute('aria-selected', 'false');
  await expect(say).not.toBeVisible();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls);
  expect(calls).toEqual([]);
});

test('only the uncertain stage asks for a choice and a 200 percent timing card stays usable', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { utterances: unknown[]; sequence: number }; calls: unknown[] } }).targetFixture;
    ipcMain.removeHandler('intercat:resolve');
    ipcMain.handle('intercat:resolve', (_event, request) => {
      fixture.calls.push(request);
      const word = fixture.state.utterances[0] as { revision: number };
      const updated = { ...word, revision: word.revision + 1, state: 'placed', review: null };
      fixture.state.utterances = [updated]; fixture.state.sequence++;
      return updated;
    });
    const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2);
  });
  const cases = [
    { stage: 'trim', reason: 'Stop A now?', label: 'Stop and send', kind: 'stop-and-send' },
    { stage: 'boundary', reason: 'When should this run?', label: 'When I say', kind: 'when-told' },
    { stage: 'operation', reason: 'Send to A as an instruction?', label: 'Send to A as an instruction', kind: 'instruction' },
    { stage: 'stale-turn', reason: 'The turn it was for has ended', label: 'Send to A as an instruction', kind: 'instruction' },
    { stage: 'command', reason: 'Which action should I prepare?', label: 'Commit', kind: 'command', command: 'commit' },
  ];
  for (const [index, entry] of cases.entries()) {
    if (index > 0) {
      await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
      await page.mouse.move(5, 5);
      await expect(page.locator('.attention-card.answered')).toHaveCount(0);
    }
    await app.evaluate((_, { index, entry }) => {
      const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { utterances: unknown[]; sequence: number } } }).targetFixture;
      fixture.state.utterances = [{ id: `review-${index}`, text: '確認が必要な部分だけを選びます。'.repeat(20), createdAt: Date.now(), attachments: [], state: 'asked', revision: 1,
        target: { mode: 'lock', trackId: 'A' }, decision: { kind: 'schedule', trackId: 'A', boundary: null, command: null, confidence: .7, inferenceMs: 20, timedOut: false },
        review: { stage: entry.stage, turn: { sessionId: 'session', turnId: 'turn' } }, reason: entry.reason }];
      fixture.state.sequence++;
    }, { index, entry });
    const card = page.getByRole('group', { name: 'Request needs attention' });
    await expect(card.getByText(entry.reason, { exact: true })).toBeVisible();
    if (entry.stage === 'trim') {
      await mkdir('research/verify/m2-review-20261105/layout', { recursive: true });
      const dimensions = await page.evaluate(() => ({ viewport: { width: innerWidth, height: innerHeight }, elements: [...document.querySelectorAll('.app,.main,.desk,.desk-scroll,.stage,.composer,.request-question,.request-options')].map(element => ({ name: element.className, bounds: element.getBoundingClientRect().toJSON(), scrollHeight: element.scrollHeight, scrollTop: element.scrollTop })) }));
      await writeFile('research/verify/m2-review-20261105/layout/dimensions.json', JSON.stringify(dimensions, null, 2));
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile('research/verify/m2-review-20261105/layout/review-200.png', Buffer.from(pixels, 'base64'));
    }
    await expect(card.getByRole('button', { name: 'Worker A', exact: true })).toHaveCount(0);
    if (entry.stage === 'boundary') {
      await card.locator('summary').focus();
      await page.keyboard.press('Space');
      await expect(card.locator('details')).toHaveAttribute('open', '');
      const full = card.getByLabel('Full request', { exact: true });
      await full.focus();
      await page.keyboard.press('End');
      await expect.poll(() => full.evaluate(element => element.scrollTop)).toBeGreaterThan(0);
      await card.locator('summary').focus();
      await page.keyboard.press('Space');
    }
    if (entry.stage === 'trim') {
      const button = card.getByRole('button', { name: entry.label, exact: true });
      await expect(button).toBeInViewport();
      const bounds = (await button.boundingBox())!;
      const scroll = (await page.locator('.desk-scroll').boundingBox())!;
      expect(bounds.y).toBeGreaterThanOrEqual(scroll.y);
      expect(bounds.y + bounds.height).toBeLessThanOrEqual(scroll.y + scroll.height);
    }
    await card.getByRole('button', { name: entry.label, exact: true }).focus();
    await expect(card.getByRole('button', { name: entry.label, exact: true })).toBeFocused();
    await expect(card.getByRole('button', { name: entry.label, exact: true })).toBeInViewport();
    if (entry.stage === 'boundary') {
      await expect(card.getByRole('button', { name: 'Stop A and send', exact: true })).toBeVisible();
      await expect(card.getByRole('button', { name: 'Stop and send', exact: true })).toHaveCount(0);
    }
    await card.getByRole('button', { name: entry.label, exact: true }).click();
    await expect(card).toHaveCount(0);
  }
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: { resolution: unknown }[] } }).targetFixture.calls);
  expect(calls.map(call => call.resolution)).toEqual(cases.map(({ kind, command }) => ({ action: 'confirm', choice: { kind, ...(command ? { command } : {}) } })));
});

test('placed requests use chips and an unconfirmed stop remains a question card', async () => {
  const page = await app.firstWindow();
  await app.evaluate(() => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { utterances: unknown[]; sequence: number } } }).targetFixture;
    const base = { createdAt: Date.now(), attachments: [], revision: 1, target: { mode: 'none' }, reason: null };
    const decision = { command: null, boundary: 'after-turn', confidence: .9, inferenceMs: 10, timedOut: false };
    fixture.state.utterances = [
      { ...base, id: 'new', text: 'Start another task', state: 'placed', decision: { ...decision, kind: 'fork', trackId: 'A' } },
      { ...base, id: 'continue', text: 'Continue the second task', state: 'placed', decision: { ...decision, kind: 'extend', trackId: 'B' } },
      { ...base, id: 'stop', text: 'Perhaps stop this', state: 'asked', decision: { ...decision, kind: 'trim', trackId: 'A' }, review: { stage: 'trim', turn: null }, reason: 'Stop A now?' },
    ];
    fixture.state.sequence++;
  });
  await expect(page.getByRole('button', { name: /Start another task/ }).filter({ has: page.locator('.request-text') })).toContainText('new track');
  await expect(page.getByRole('button', { name: /Continue the second task/ }).filter({ has: page.locator('.request-text') })).toContainText('to B');
  await expect(page.locator('.request-chip')).toHaveCount(2);
  const stop = page.getByRole('group', { name: 'Request needs attention', exact: true });
  await expect(stop).toContainText('Stop A now?');
  await expect(stop).toContainText('Not started');
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls);
  expect(calls).toEqual([]);
});

test('a late recommendation preserves the card and existing controls and appends an outside candidate', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain }) => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { tracks: unknown[]; utterances: unknown[]; sequence: number }; calls: unknown[] } }).targetFixture;
    fixture.state.tracks = 'ABCDEFGHI'.split('').map(label => ({ id: label, label, title: `Dummy task ${label}`, status: 'waiting' }));
    fixture.state.utterances = [{ id: 'late-dummy', text: 'Choose the dummy task', attachments: [], createdAt: 1, state: 'asked', revision: 1,
      target: { mode: 'none' }, decision: null, reason: 'The Editor took too long. Choose where this should go.', deadline: { trackId: null, recommendation: null } }];
    fixture.state.sequence++;
    ipcMain.removeHandler('intercat:resolve');
    ipcMain.handle('intercat:resolve', (_event, request) => { fixture.calls.push(request); return fixture.state.utterances[0]; });
  });
  const card = page.getByRole('group', { name: 'Request needs attention' });
  const newTrack = card.getByRole('button', { name: 'New track', exact: true });
  await expect(newTrack).toBeVisible();
  await expect(card.getByRole('button', { name: 'Worker I', exact: true })).toHaveCount(0);
  await newTrack.focus();
  const before = await newTrack.boundingBox();
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { targetFixture: { state: { utterances: { deadline: { recommendation: unknown } }[]; sequence: number } } }).targetFixture.state;
    state.utterances[0].deadline.recommendation = { trackId: 'I' }; state.sequence++;
  });
  const recommended = card.getByRole('button', { name: 'Worker I recommended', exact: true });
  await expect(recommended).toBeVisible();
  await expect(newTrack).toBeFocused();
  expect(await newTrack.boundingBox()).toEqual(before);
  await expect(card).toContainText('Not started · pick a track');
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls)).toEqual([]);
  await mkdir('research/verify/m2-late-decisions-20261107', { recursive: true });
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/m2-late-decisions-20261107/recommended-100.png', Buffer.from(pixels, 'base64'));
  await recommended.click();
  const calls = await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls);
  expect(calls).toHaveLength(1);
  expect(calls[0]).toMatchObject({ utteranceId: 'late-dummy', revision: 1, resolution: { action: 'retry', target: { mode: 'lock', trackId: 'I' } } });
});

test('a locked deadline offers instruction or hold and new-track recommendations fit at 200 percent', async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }) => {
    const fixture = (globalThis as typeof globalThis & { targetFixture: { state: { utterances: unknown[]; sequence: number }; calls: unknown[] } }).targetFixture;
    fixture.state.utterances = [{ id: 'locked-dummy', text: 'Dummy instruction for A', attachments: [], createdAt: 1, state: 'asked', revision: 1,
      target: { mode: 'lock', trackId: 'A' }, decision: null, reason: 'The Editor took too long. Choose where this should go.', deadline: { trackId: 'A', recommendation: null } }];
    fixture.state.sequence++;
    ipcMain.removeHandler('intercat:resolve');
    ipcMain.handle('intercat:resolve', (_event, request) => { fixture.calls.push(request); return fixture.state.utterances[0]; });
    const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2);
  });
  const card = page.getByRole('group', { name: 'Request needs attention' });
  const instruction = card.getByRole('button', { name: 'Send to A as an instruction', exact: true });
  await expect(instruction).toBeVisible();
  await expect(card.getByRole('button', { name: 'Worker A', exact: true })).toHaveCount(0);
  await expect(card.getByRole('button', { name: 'Hold', exact: true })).toBeVisible();
  await instruction.focus();
  await expect(instruction).toBeInViewport();
  await instruction.click();
  expect(await app.evaluate(() => (globalThis as typeof globalThis & { targetFixture: { calls: unknown[] } }).targetFixture.calls)).toEqual([
    expect.objectContaining({ utteranceId: 'locked-dummy', revision: 1, resolution: { action: 'confirm', choice: { kind: 'instruction' } } }),
  ]);
  await app.evaluate(() => {
    const state = (globalThis as typeof globalThis & { targetFixture: { state: { utterances: unknown[]; sequence: number } } }).targetFixture.state;
    state.utterances = [{ id: 'new-dummy', text: 'Dummy new task', attachments: [], createdAt: 1, state: 'asked', revision: 1,
      target: { mode: 'none' }, decision: null, reason: 'The Editor took too long. Choose where this should go.', deadline: { trackId: null, recommendation: { trackId: null } } }]; state.sequence++;
  });
  await page.mouse.move(5, 5);
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  const newTrack = page.getByRole('group', { name: 'Request needs attention' }).getByRole('button', { name: 'New track recommended', exact: true });
  await expect(newTrack).toBeVisible();
  await newTrack.focus();
  await expect(newTrack).toBeInViewport();
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile('research/verify/m2-late-decisions-20261107/recommended-200.png', Buffer.from(pixels, 'base64'));
});


for (const zoom of [1, 2]) test(`an explicit new-track deadline has a direct confirmation at ${zoom * 100}%`, async () => {
  const page = await app.firstWindow();
  await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
    const fixture = (globalThis as any).targetFixture;
    fixture.state.utterances = [{ id: 'new-deadline', text: 'この新しい仕事を始めてください。'.repeat(12), attachments: [], createdAt: 1, state: 'asked', revision: 1,
      target: { mode: 'new' }, decision: null, reason: 'The Editor took too long. Start a new track now?', deadline: { trackId: null, recommendation: null } }];
    fixture.state.sequence++;
    ipcMain.removeHandler('intercat:resolve');
    ipcMain.handle('intercat:resolve', (_event, request) => { fixture.calls.push(request); return fixture.state.utterances[0]; });
    const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
  }, zoom);
  const card = page.getByRole('group', { name: 'Request needs attention' });
  const confirm = card.getByRole('button', { name: 'Start a new track now', exact: true });
  await expect(confirm).toBeVisible();
  await expect(card.getByRole('button', { name: 'New track', exact: true })).toHaveCount(0);
  await expect(card.getByRole('button', { name: 'Worker A', exact: true })).toHaveCount(0);
  await confirm.focus(); await expect(confirm).toBeInViewport(); await confirm.click();
  expect(await app.evaluate(() => (globalThis as any).targetFixture.calls)).toEqual([
    expect.objectContaining({ utteranceId: 'new-deadline', revision: 1, resolution: { action: 'confirm', choice: { kind: 'instruction' } } }),
  ]);
});
