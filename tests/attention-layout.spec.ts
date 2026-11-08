import { _electron as electron, expect, test, type ElectronApplication, type Locator, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { Assumption, AssumptionCommand, DaemonStatus, InputResolution, Utterance, WorkerState } from '../src/bridge';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

let app: ElectronApplication;
let page: Page;
let directory: string;
let errors: string[];
const choice: Assumption = {
  id: 'layout-assumption', source: 'default-placement', sourceId: 'dummy-report', clipId: 'dummy-clip', trackId: 'A',
  question: '一覧を保存する形式には何を使いますか。', options: ['JSON', 'TSV'], text: 'JSON',
  evidence: '形式の指定がないため、JSONで保存して確認を進めました。', revision: 1,
  state: 'open', keptAt: null, changedAt: null, changeInputId: null,
};
const request = (stage: 'lane' | 'command' | 'boundary'): Utterance => ({
  id: 'layout-request', text: '現在の処理を確認し、結果をどこへ置くか選択してください。'.repeat(20),
  attachments: [], createdAt: 1, state: 'asked', revision: 1, target: { mode: 'none' },
  decision: stage === 'boundary' ? { kind: 'schedule', trackId: 'A', boundary: 'when-told', command: null, confidence: 0.5, inferenceMs: 10, timedOut: false } : null,
  review: { stage, turn: null }, reason: 'この依頼を進める方法を確認してください。',
});
const worker = (id: string): WorkerState => ({
  trackId: id, phase: 'ready', sessionId: `dummy-session-${id}`, activeTurn: null,
  identity: null, detail: null, checkpointPending: false, checkpoint: null, approvals: [], prompts: [], output: null,
});

test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/ic-attention-layout-');
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}`] });
  await mockDrafts(app);
  page = await app.firstWindow();
  errors = [];
  page.on('pageerror', error => errors.push(error.message));
  const now = Date.now();
  await page.clock.install({ time: now });
  await page.clock.pauseAt(now + 1000);
  await app.evaluate(({ ipcMain }, initial) => {
    const state: DaemonStatus = {
      connected: true, sequence: 1, workerConfigured: true,
      session: { id: 'layout-session', name: 'Dummy card layout', slug: 'layout', createdAt: 1 },
      tracks: 'ABCDEFGH'.split('').map(id => ({ id, label: id, title: `架空の見本 ${id}`, status: 'ready' })),
      workers: [], assumptions: [initial.choice], utterances: [initial.request],
    };
    const fixture = { state, resolutions: [] as unknown[], assumptions: [] as unknown[] };
    Object.assign(globalThis, { attentionLayout: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:resolve'); ipcMain.handle('intercat:resolve', (_event, command: { utteranceId: string; resolution: InputResolution }) => {
      fixture.resolutions.push(command);
      const word = state.utterances!.find(word => word.id === command.utteranceId)!;
      word.state = 'held'; word.revision++; state.sequence!++;
      return word;
    });
    ipcMain.removeHandler('intercat:assumption-answer'); ipcMain.handle('intercat:assumption-answer', (_event, command: AssumptionCommand) => {
      fixture.assumptions.push(command); state.assumptions = []; state.sequence!++;
      return { ...initial.choice, state: 'kept' };
    });
  }, { choice, request: request('lane') });
  await page.clock.runFor(1100);
  await leave();
});

test.afterEach(async () => {
  try { expect(errors).toEqual([]); }
  finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});

async function leave() {
  await page.getByRole('textbox', { name: 'Request draft', exact: true }).focus();
  await page.mouse.move(5, 5);
  await page.clock.runFor(1600);
}
async function update(change: Partial<DaemonStatus>) {
  await app.evaluate((_electron, change) => {
    const fixture = (globalThis as typeof globalThis & { attentionLayout: { state: DaemonStatus } }).attentionLayout;
    Object.assign(fixture.state, change); fixture.state.sequence!++;
  }, change);
  await page.clock.runFor(1100);
}
async function reachable(control: Locator) {
  await control.scrollIntoViewIfNeeded();
  await page.clock.runFor(32);
  await expect(control).toBeInViewport();
  expect(await control.evaluate(element => {
    const box = element.getBoundingClientRect();
    const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
    return hit === element || !!hit && element.contains(hit);
  })).toBe(true);
}
async function screenshot(name: string) {
  const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(test.info().outputPath(name), Buffer.from(pixels, 'base64'));
}

for (const zoom of [1, 2]) {
  test(`mixed cards preserve focus, priority, and reachable actions at ${zoom * 100} percent`, async () => {
    await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1300, 1000); });
    await page.clock.runFor(64);
    const assumption = page.locator('[data-attention-card="assumption-layout-assumption"]');
    const question = page.locator('[data-attention-card="request-layout-request"]');
    await assumption.locator('summary').click();
    const keep = assumption.getByRole('button', { name: 'Keep', exact: true });
    await reachable(keep);
    await keep.focus();
    await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
    await page.clock.runFor(64);
    await reachable(keep);
    const before = (await keep.boundingBox())!;
    const stopped = worker('B');
    stopped.phase = 'question';
    stopped.prompts = [{ request: { sessionId: stopped.sessionId!, userInputId: 'three-questions', turnId: 'dummy-turn', questions: [1, 2, 3].map(index => ({
      id: `q${index}`, header: `保存形式 ${index}`, question: 'この処理の保存形式を選び、既存の出力に与える影響を確認してください。'.repeat(4),
      selection: { mode: 'single' as const }, options: [{ label: `JSON ${index}` }, { label: `TSV ${index}` }],
    })) }, revision: '1', phase: 'open', operation: null }];
    await update({ workers: [stopped] });
    await expect(keep).toBeFocused();
    expect(Math.abs((await keep.boundingBox())!.y - before.y)).toBeLessThan(1);
    await expect(page.getByText('+1 new', { exact: true })).toBeVisible();
    await expect(page.locator('[data-attention-card="prompt-B-three-questions"]')).toBeHidden();
    await leave();
    await expect(question).toBeHidden();
    await expect(page.locator('[data-attention-card="prompt-B-three-questions"]')).toBeVisible();
    await page.getByRole('button', { name: /\+\d+ more/ }).click();
    await expect(question).toBeVisible();
    const prompt = page.getByRole('group', { name: 'Questions for Worker B', exact: true });
    for (let index = 1; index <= 3; index++) await prompt.getByRole('radio', { name: `JSON ${index}`, exact: true }).check();
    await expect(prompt.getByRole('button', { name: 'Send answers', exact: true })).toBeEnabled();
    for (const control of await prompt.locator('[data-card-actions] button').all()) await reachable(control);
    await screenshot(`mixed-${zoom * 100}.png`);
    await leave();
    await reachable(question.getByRole('button', { name: 'Hold', exact: true }));
    await question.getByRole('button', { name: 'Hold', exact: true }).click();
    await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { attentionLayout: { resolutions: unknown[] } }).attentionLayout.resolutions)).toEqual([
      expect.objectContaining({ utteranceId: 'layout-request', resolution: { action: 'hold' } }),
    ]);
    await expect(keep).toBeEnabled();
    await reachable(keep);
    await keep.click();
    await expect.poll(() => app.evaluate(() => (globalThis as typeof globalThis & { attentionLayout: { assumptions: unknown[] } }).attentionLayout.assumptions)).toEqual([
      expect.objectContaining({ assumptionId: choice.id, answer: { action: 'keep' } }),
    ]);
    await expect(prompt).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });
}

test('all command and timing choices remain reachable beside an assumption at 200 percent', async () => {
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  await page.clock.runFor(100);
  if (await page.getByRole('button', { name: /\+1 more/ }).isVisible()) await page.getByRole('button', { name: /\+1 more/ }).click();
  for (const stage of ['lane', 'command', 'boundary'] as const) {
    await leave();
    await update({ utterances: [{ ...request(stage), revision: stage === 'lane' ? 2 : stage === 'command' ? 3 : 4 }] });
    const card = page.locator('[data-attention-card="request-layout-request"]');
    const ready = stage === 'lane' ? 'Worker A' : stage === 'command' ? 'Commit' : 'After this turn';
    await expect.poll(async () => {
      await page.clock.runFor(64);
      return card.getByRole('button', { name: ready, exact: true }).count();
    }).toBe(1);
    for (const button of await card.locator('[data-card-actions] button').all()) await reachable(button);
    const boxes = await card.evaluate(element => {
      const box = element.getBoundingClientRect();
      return { width: box.width, scrollWidth: element.scrollWidth, clientWidth: element.clientWidth };
    });
    expect(boxes.scrollWidth).toBeLessThanOrEqual(boxes.clientWidth + 1);
    await screenshot(`${stage}-200.png`);
  }
});

test('card capacity follows available space and keeps every folded card reachable', async () => {
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1300, 1000); window.webContents.setZoomFactor(1); });
  await leave();
  const visible = page.locator('[data-attention-card]:visible');
  await expect(visible).toHaveCount(2);
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(2); });
  await page.clock.runFor(100);
  await expect(visible).toHaveCount(1);
  await expect(page.locator('[data-attention-card="request-layout-request"]')).toBeVisible();
  await screenshot('compact-200.png');
  await page.getByRole('button', { name: /\+1 more/ }).click();
  await expect(visible).toHaveCount(2);
  await screenshot('compact-expanded-200.png');
  await page.getByRole('button', { name: 'Show fewer cards', exact: true }).click();
  await expect(visible).toHaveCount(1);
  await leave();
  await app.evaluate(({ BrowserWindow }) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1300, 1000); window.webContents.setZoomFactor(1); });
  await expect.poll(async () => { await page.clock.runFor(64); return visible.count(); }).toBe(2);
  await writeFile(test.info().outputPath('restored-layout.json'), JSON.stringify(await page.evaluate(() => ({
    viewport: { width: innerWidth, height: innerHeight },
    focus: document.activeElement?.outerHTML,
    nodes: ['.main', '.stage', '.desk', '.desk-scroll', '.needs-you', '.needs-you-more', '[data-attention-card="request-layout-request"]', '[data-attention-card="assumption-layout-assumption"]'].map(selector => {
      const element = document.querySelector<HTMLElement>(selector);
      return { selector, rect: element?.getBoundingClientRect().toJSON(), scrollHeight: element?.scrollHeight, display: element && getComputedStyle(element).display };
    }),
  })), null, 2));
  await expect(visible).toHaveCount(2);
  await page.clock.runFor(500);
  await expect(visible).toHaveCount(2);
  await update({ workers: ['A', 'B', 'C'].map(id => ({ ...worker(id), output: 'これは表示領域を使う架空の作業報告です。\n'.repeat(20) })) });
  await expect(visible).toHaveCount(1);
  await update({ workers: [] });
  await expect(visible).toHaveCount(2);
});
