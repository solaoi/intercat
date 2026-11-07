import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

async function setup(options: { fail?: boolean; zoom?: number; many?: boolean } = {}) {
  const directory = await mkdtemp('/private/tmp/ic-privacy-view-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  await app.evaluate(({ ipcMain }, options) => {
    const word = { id: 'privacy-request', text: 'Contact dummy@example.invalid. Review second-private.', attachments: [], state: 'placed', createdAt: Date.now(), revision: 1,
      target: { mode: 'none' }, decision: { kind: 'extend', trackId: 'A', boundary: 'after-turn' }, reason: null, editHold: null as object | null };
    const state = { connected: true, sequence: 1, privacyRevision: 1, workerConfigured: true, workers: [], utterances: options.many ? [word, { ...word, id: 'second', text: 'Second contact dummy@example.invalid' }, { ...word, id: 'third', text: 'Third contact dummy@example.invalid' }] : [word],
      tracks: [{ id: 'A', label: 'A', title: 'Dummy task', status: 'waiting' }] };
    const fixture = { state, fail: !!options.fail, maskExtra: false, delay: false, releases: [] as (() => void)[], originals: [] as number[], batches: [] as string[][] };
    Object.assign(globalThis, { privacyFixture: fixture });
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
    ipcMain.removeHandler('intercat:privacy-preview'); ipcMain.handle('intercat:privacy-preview', (_event, texts: string[]) => {
      fixture.batches.push(texts);
      if (fixture.fail) throw new Error('Dummy privacy scan failure');
      const views = texts.map(text => {
        const parts: ({ kind: 'text'; text: string } | { kind: 'redaction'; id: number; category: string })[] = [];
        let previous = 0;
        for (const match of text.matchAll(fixture.maskExtra ? /dummy@example\.invalid|second-private/g : /dummy@example\.invalid/g)) {
          if (match.index! > previous) parts.push({ kind: 'text', text: text.slice(previous, match.index) });
          parts.push({ kind: 'redaction', id: match[0] === 'second-private' ? 2 : 1, category: match[0] === 'second-private' ? 'token' : 'email' });
          previous = match.index! + match[0].length;
        }
        if (previous < text.length) parts.push({ kind: 'text', text: text.slice(previous) });
        return { text: parts.map(part => part.kind === 'text' ? part.text : `<redacted:${part.category}#${part.id}>`).join(''), parts };
      });
      return fixture.delay ? new Promise(resolve => { fixture.releases.push(() => resolve(views)); }) : views;
    });
    ipcMain.removeHandler('intercat:privacy-original'); ipcMain.handle('intercat:privacy-original', (_event, id: number) => { fixture.originals.push(id); return id === 1 ? 'dummy@example.invalid' : 'second-private'; });
    ipcMain.removeHandler('intercat:edit-request'); ipcMain.handle('intercat:edit-request', (_event, action) => {
      if (action.kind === 'begin') { word.revision++; word.editHold = { owner: 'dummy-window', token: action.token, revision: word.revision }; }
      if (action.kind === 'end') { word.revision++; word.editHold = null; }
      state.sequence++; return word;
    });
  }, options);
  const page = await app.firstWindow();
  if (options.zoom) await app.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, options.zoom);
  await page.reload();
  return { app, page, cleanup: async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); } };
}

async function openDetails(page: Awaited<ReturnType<typeof setup>>['page']) {
  await page.locator('.request-select').first().click();
  const dialog = page.getByRole('dialog', { name: 'Request', exact: true });
  await expect(dialog).toBeVisible();
  return dialog;
}

test('requests show numbered text and only fetch the original on hover', async () => {
  const { app, page, cleanup } = await setup();
  try {
    await expect(page.locator('.request-select')).toHaveAttribute('title', 'Contact <redacted:email#1>. Review second-private.');
    const dialog = await openDetails(page);
    const value = dialog.getByRole('button', { name: 'Show original email value' });
    await expect(value).toBeVisible();
    expect(await app.evaluate(() => (globalThis as any).privacyFixture.originals)).toEqual([]);
    await expect(page.getByText('dummy@example.invalid', { exact: true })).toHaveCount(0);
    await value.hover();
    await expect(page.getByRole('tooltip')).toHaveText('dummy@example.invalid');
    await page.getByRole('tooltip').hover(); await expect(page.getByRole('tooltip')).toBeVisible();
    await dialog.getByRole('heading', { name: 'You said' }).hover();
    await expect(page.getByRole('tooltip')).toHaveCount(0);
  } finally { await cleanup(); }
});

test('keyboard reveal closes before the request popover', async () => {
  const { page, cleanup } = await setup();
  try {
    const dialog = await openDetails(page);
    const value = dialog.getByRole('button', { name: 'Show original email value' });
    await value.focus(); await expect(page.getByRole('tooltip')).toHaveText('dummy@example.invalid');
    await page.keyboard.press('Escape'); await expect(page.getByRole('tooltip')).toHaveCount(0);
    await expect(dialog).toBeVisible();
    await page.keyboard.press('Escape'); await expect(dialog).toHaveCount(0);
  } finally { await cleanup(); }
});

test('copy rechecks the source and never writes the original value to the clipboard', async () => {
  const { app, page, cleanup } = await setup();
  try {
    await page.evaluate(() => { Object.assign(window, { copiedPrivacy: [] }); Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async (text: string) => { (window as any).copiedPrivacy.push(text); } } }); });
    const dialog = await openDetails(page);
    await expect(dialog.getByRole('button', { name: 'Show original email value' })).toBeVisible();
    await app.evaluate(() => { (globalThis as any).privacyFixture.maskExtra = true; });
    await dialog.getByRole('button', { name: 'Copy masked text', exact: true }).click();
    await expect.poll(() => page.evaluate(() => (window as any).copiedPrivacy)).toEqual(['Contact <redacted:email#1>. Review <redacted:token#2>.']);
  } finally { await cleanup(); }
});

test('a failed preview hides the raw request and retries without changing it', async () => {
  const { app, page, cleanup } = await setup({ fail: true });
  try {
    const dialog = await openDetails(page);
    await expect(dialog.locator('.request-original .privacy-pending')).toContainText('Privacy check unavailable');
    await expect(page.getByText('dummy@example.invalid', { exact: false })).toHaveCount(0);
    await app.evaluate(() => { (globalThis as any).privacyFixture.fail = false; });
    await dialog.getByRole('button', { name: 'Retry privacy check' }).click();
    await expect(dialog.getByRole('button', { name: 'Show original email value' })).toBeVisible();
  } finally { await cleanup(); }
});

test('visible requests share a batch and refresh after a fingerprint revision', async () => {
  const { app, page, cleanup } = await setup({ many: true });
  try {
    await expect(page.locator('.request-text').first()).toContainText('<redacted:email#1>');
    expect(await app.evaluate(() => (globalThis as any).privacyFixture.batches.some((batch: string[]) => ['Contact ', 'Second contact ', 'Third contact '].every(prefix => batch.some(text => text.startsWith(prefix)))))).toBe(true);
    await app.evaluate(() => { const fixture = (globalThis as any).privacyFixture; fixture.maskExtra = true; fixture.state.privacyRevision++; fixture.state.sequence++; });
    await expect(page.locator('.request-text').first()).toContainText('<redacted:token#2>');
  } finally { await cleanup(); }
});

for (const zoom of [1, 2]) test(`local original remains on screen at ${zoom * 100}%`, async () => {
  const { app, page, cleanup } = await setup({ zoom });
  try {
    const dialog = await openDetails(page);
    const value = dialog.getByRole('button', { name: 'Show original email value' });
    await value.focus(); const tooltip = page.getByRole('tooltip'); await expect(tooltip).toHaveText('dummy@example.invalid');
    const bounds = await tooltip.boundingBox();
    const viewport = await page.evaluate(() => ({ width: innerWidth, height: innerHeight }));
    expect(bounds!.x).toBeGreaterThanOrEqual(0); expect(bounds!.y).toBeGreaterThanOrEqual(0);
    expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(viewport.width); expect(bounds!.y + bounds!.height).toBeLessThanOrEqual(viewport.height);
    await mkdir('research/verify/m2-privacy-view-20261107', { recursive: true });
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(`research/verify/m2-privacy-view-20261107/privacy-${zoom * 100}.png`, Buffer.from(pixels, 'base64'));
  } finally { await cleanup(); }
});

test('a late copy cannot replace the clipboard after its request closes', async () => {
  const { app, page, cleanup } = await setup();
  try {
    await page.evaluate(() => { Object.assign(window, { copiedPrivacy: [] }); Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText: async (text: string) => { (window as any).copiedPrivacy.push(text); } } }); });
    const dialog = await openDetails(page);
    await expect(dialog.getByRole('button', { name: 'Copy masked text' })).toBeVisible();
    await app.evaluate(() => { (globalThis as any).privacyFixture.delay = true; });
    await dialog.getByRole('button', { name: 'Copy masked text' }).click();
    await expect.poll(() => app.evaluate(() => (globalThis as any).privacyFixture.releases.length)).toBe(1);
    await dialog.getByRole('button', { name: 'Close request details' }).click();
    await expect(dialog).toHaveCount(0);
    await app.evaluate(() => { const fixture = (globalThis as any).privacyFixture; fixture.delay = false; for (const release of fixture.releases) release(); });
    const reopened = await openDetails(page);
    await expect(reopened.getByRole('button', { name: 'Show original email value' })).toBeVisible();
    expect(await page.evaluate(() => (window as any).copiedPrivacy)).toEqual([]);
  } finally { await cleanup(); }
});

test('history, attention cards and timeline marks use checked display copies', async () => {
  const { app, page, cleanup } = await setup();
  try {
    await app.evaluate(({ ipcMain }) => {
      const fixture = (globalThis as any).privacyFixture;
      const original = fixture.state.utterances[0];
      const now = Date.now();
      fixture.state.timeline = { clock: { epochMs: now, monoMs: 0, timelineMs: now, bootId: 'privacy-fixture' }, summaries: [] };
      fixture.state.editHistory = { undo: { entryId: 'undo-dummy', label: 'Take back', text: original.text, at: Date.now(), blocked: null }, redo: null, takenBack: [{ id: 'taken-back', text: original.text, revision: 1 }] };
      fixture.state.utterances.push(...['one', 'two', 'three'].map(id => ({ ...original, id, state: 'asked', reason: 'Check dummy@example.invalid', decision: null })));
      fixture.state.sequence++;
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window', () => ({ revision: 2, reset: true, clips: [], marks: [{ id: 'mark-dummy', inputId: null, trackId: 'A', repairRunId: null, at: now - 60000, text: original.text }], removed: [], removedMarks: [] }));
    });
    await page.reload();
    await expect(page.getByRole('button', { name: 'Undo', exact: true })).toHaveAttribute('title', /<redacted:email#1>/);
    await page.getByText('Taken back · 1', { exact: true }).click();
    await expect(page.locator('.taken-back-word')).toContainText('<redacted:email#1>');
    await expect(page.locator('.needs-you-more')).toHaveAttribute('title', /<redacted:email#1>/);
    await expect(page.locator('.input-mark-actions button')).toHaveAttribute('aria-label', /<redacted:email#1>/);
    expect(await page.locator('body').innerText()).not.toContain('dummy@example.invalid');
    expect(await page.locator('[title], [aria-label]').evaluateAll(elements => elements.some(element => [element.getAttribute('title'), element.getAttribute('aria-label')].some(value => value?.includes('dummy@example.invalid'))))).toBe(false);
  } finally { await cleanup(); }
});

test('Full display retries when the local Editor becomes ready without a policy change', async () => {
  const { app, page, cleanup } = await setup({ fail: true });
  try {
    await app.evaluate(() => {
      const f = (globalThis as any).privacyFixture;
      Object.assign(f.state, { privacy: { mode: 'full', revision: 1 }, editor: { phase: 'stopped', generation: 1, modelLoaded: false, accepting: false } });
      f.state.sequence++;
    });
    await page.reload();
    const dialog = await openDetails(page);
    await expect(dialog.locator('.privacy-pending')).toContainText('Privacy check unavailable');
    await app.evaluate(() => {
      const f = (globalThis as any).privacyFixture;
      f.fail = false;
      f.state.editor = { phase: 'ready', generation: 2, modelLoaded: true, accepting: true };
      f.state.sequence++;
    });
    await expect(dialog.getByRole('button', { name: 'Show original email value' })).toBeVisible();
  } finally { await cleanup(); }
});

test('the static demo keeps its sample labels without calling the real privacy endpoint', async () => {
  const directory = await mkdtemp('/private/tmp/ic-privacy-demo-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain }) => {
      Object.assign(globalThis, { demoPrivacyCalls: 0 });
      ipcMain.removeHandler('intercat:privacy-preview');
      ipcMain.handle('intercat:privacy-preview', () => { (globalThis as any).demoPrivacyCalls++; throw new Error('Real privacy endpoint is unavailable in the static demo.'); });
    });
    const page = await app.firstWindow(); await page.reload();
    await expect(page.getByRole('heading', { name: 'Login hardening', exact: true })).toBeVisible();
    await expect(page.getByRole('combobox', { name: 'Request destination', exact: true })).toContainText('Only A');
    await expect(page.locator('.desk')).not.toContainText('Privacy check unavailable');
    expect(await app.evaluate(() => (globalThis as any).demoPrivacyCalls)).toBe(0);
  } finally { await closeWindow(app); await rm(directory, { recursive: true, force: true }); }
});

test('the native app shows the local preview and reveals its original without a worker call', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon and Keychain');
  test.setTimeout(180_000);
  const directory = await mkdtemp('/private/tmp/ic-native-privacy-view-');
  const app = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
  let pid: number | undefined;
  try {
    const page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await page.evaluate(async () => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text: 'Review dummy@example.invalid', target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }));
    await expect(page.locator('.request-select')).toContainText('<redacted:email#1>', { timeout: 30_000 });
    const dialog = await openDetails(page);
    const value = dialog.getByRole('button', { name: 'Show original email value' });
    await value.focus(); await expect(page.getByRole('tooltip')).toHaveText('dummy@example.invalid');
    const status = await page.evaluate(() => window.intercat.getStatus());
    expect(status.workers).toEqual([]);
    await mkdir('research/verify/m2-privacy-view-20261107', { recursive: true });
    await writeFile('research/verify/m2-privacy-view-20261107/native.json', JSON.stringify(status, null, 2));
  } finally {
    await closeWindow(app);
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    await rm(directory, { recursive: true, force: true });
  }
});

test('Editor reactions and their saved log use the same private-value display', async () => {
  const {app,page,cleanup}=await setup();
  try {
    await app.evaluate(({ipcMain})=>{
      const fixture=(globalThis as any).privacyFixture;
      const entry={id:1,trackId:'A',inputId:null,scene:'assumed',title:'Recorded choice',line:'A used dummy@example.invalid.',detail:'The recorded choice was dummy@example.invalid.',createdAt:Date.now(),expiresAt:null};
      fixture.state.editorEntries=[entry];fixture.state.sequence++;
      ipcMain.removeHandler('intercat:editor-log');ipcMain.handle('intercat:editor-log',()=>({entries:[entry],before:null}));
    });
    const line=page.getByRole('region',{name:'Editor explanation',exact:true});
    await expect(line).toContainText('<redacted:email#1>');
    await expect(line).not.toContainText('dummy@example.invalid');
    await line.getByRole('button',{name:'more ›'}).click();
    const detail=page.getByRole('dialog',{name:'Editor explanation details'});
    await expect(detail.getByRole('button',{name:'Show original email value'})).toBeVisible();
    expect(await app.evaluate(()=>(globalThis as any).privacyFixture.originals)).toEqual([]);
    await page.keyboard.press('Escape');
    await page.locator('.editor-log > summary').click();
    const log=page.getByRole('region',{name:'Editor log for Worker A'});
    await expect(log.getByRole('button',{name:'Show original email value'})).toBeVisible();
    await expect(log).not.toContainText('dummy@example.invalid');
    expect(await app.evaluate(()=>(globalThis as any).privacyFixture.originals)).toEqual([]);
    await log.getByRole('button',{name:'Show original email value'}).hover();
    await expect(page.getByRole('tooltip')).toHaveText('dummy@example.invalid');
  } finally {await cleanup();}
});
