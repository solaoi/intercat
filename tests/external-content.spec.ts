import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { promisify } from 'node:util';

const execute = promisify(execFile);
const evidence = path.resolve('research/verify/external-content-20261104');
let data: string;
let daemon: ChildProcess;
let token: string;
let app: ElectronApplication;
const pixel = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a4PcAAAAASUVORK5CYII=', 'base64');
test.beforeAll(async () => {
  await mkdir(evidence, { recursive: true });
  data = await mkdtemp('/private/tmp/ic-external-');
  await execute('cargo', ['build', '--offline', '-q', '--bin', 'intercatd']);
  token = randomBytes(32).toString('hex');
  daemon = spawn(path.resolve(process.env.CARGO_TARGET_DIR ?? 'target', 'debug/intercatd'), ['serve', data, path.resolve('services/editor/src')], { stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env, INTERCAT_GITLEAKS: path.resolve('.tools/gitleaks-8.30.1/gitleaks') } });
  let error = '';
  daemon.stderr!.on('data', chunk => { error += chunk.toString(); });
  const lines = createInterface({ input: daemon.stdout! });
  const hello = (async () => {
    for await (const line of lines) {
      const value = JSON.parse(line);
      if (!value.fingerprints) throw new Error('Missing test bootstrap');
      daemon.stdin!.end(JSON.stringify(value.fingerprints.catalog) + '\n'); lines.close(); return;
    }
    throw new Error(`Daemon did not start: ${error}`);
  })();
  daemon.stdin!.write(JSON.stringify({ token, secretKey: Buffer.alloc(32, 7).toString('hex'), receipt: null }) + '\n');
  await hello;
  await expect.poll(async () => { try { return (await stat(path.join(data, 'run/rpc.sock'))).isSocket(); } catch { return false; } }).toBe(true);
});
test.afterAll(async () => {
  if (daemon && daemon.exitCode === null) { const stopped = new Promise(resolve => daemon.once('exit', resolve)); daemon.kill('SIGTERM'); await stopped; }
  if (data) await rm(data, { recursive: true, force: true });
});
test.beforeEach(async () => {
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${path.join(data, randomBytes(8).toString('hex'))}`] });
  await mockDrafts(app);
  await app.firstWindow();
});
test.afterEach(async () => { if (app) await closeWindow(app); });

async function report(text: string): Promise<Page> {
  await app.evaluate(async ({ ipcMain, BrowserWindow }, input) => {
    const load = process.getBuiltinModule('module').createRequire(input.client);
    const { RpcClient } = load(input.client);
    const { reviewExternal, confirmExternal } = load(input.external);
    const client = await RpcClient.connect(input.socket, input.token);
    const previous = (globalThis as typeof globalThis & { externalState?: { sequence: number } }).externalState;
    const state = { connected: true, sequence: (previous?.sequence ?? 0) + 1, workerConfigured: true, tracks: [{ id: 'a', label: 'A', title: 'External image report', status: 'done' }], workers: [{ trackId: 'a', phase: 'ready', sessionId: 'session', identity: null, activeTurn: null, checkpointPending: false, checkpoint: null, prompts: [], approvals: [], output: input.text }], workspaces: [], utterances: [] };
    const memory = globalThis as typeof globalThis & { externalState: typeof state; reviews: { id: string; kind: string }[]; opened: string[] };
    memory.externalState = state; memory.reviews = []; memory.opened = [];
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status', () => state);
    ipcMain.removeHandler('intercat:external-review'); ipcMain.handle('intercat:external-review', async (_event, request) => { const review = await reviewExternal(client, request); memory.reviews.push(review); return review; });
    ipcMain.removeHandler('intercat:external-confirm'); ipcMain.handle('intercat:external-confirm', (_event, request) => confirmExternal(client, request, async (url: string) => { memory.opened.push(url); }));
    ipcMain.removeHandler('intercat:external-cancel'); ipcMain.handle('intercat:external-cancel', (_event, id) => client.request('external.cancel', { id }));
    BrowserWindow.getAllWindows()[0].setSize(1040, 760);
  }, { text, client: path.resolve('electron/rpc-client.mjs'), external: path.resolve('electron/external-content.mjs'), socket: path.join(data, 'run/rpc.sock'), token });
  const page = await app.firstWindow();
  await expect(page.getByRole('log', { name: 'Worker A result' })).toBeVisible();
  const alt = /!\[([^\]]*)\]/.exec(text)?.[1];
  if (alt) await expect(page.locator('.external-image-label').first()).toContainText(alt);
  return page;
}
async function server(handler: (request: IncomingMessage, response: ServerResponse) => void) {
  const value = createServer(handler);
  await new Promise<void>(resolve => value.listen(0, '127.0.0.1', resolve));
  const address = value.address();
  if (!address || typeof address === 'string') throw new Error('Missing server port');
  return { url: `http://127.0.0.1:${address.port}`, close: () => new Promise<void>((resolve, reject) => value.close(error => error ? reject(error) : resolve())) };
}
async function load(page: Page) {
  await page.getByRole('button', { name: /^Load image from/ }).click();
  const confirm = page.getByRole('dialog', { name: 'Confirm image URL' });
  await expect(confirm).toBeVisible();
  return confirm;
}

test('image URLs stay offline until reviewed, cancelled reviews stay offline, and confirmation fetches once', async () => {
  const requests: { path: string; cookie?: string; authorization?: string; referer?: string }[] = [];
  const http = await server((request, response) => { requests.push({ path: request.url!, cookie: request.headers.cookie, authorization: request.headers.authorization, referer: request.headers.referer }); response.writeHead(200, { 'Content-Type': 'image/png' }); response.end(pixel); });
  try {
    const url = `${http.url}/image.png?label=preview#detail`;
    const page = await report(`## Verified report\n\n**Changes** are ready.\n\n![Result image](${url})\n\n<img src="${http.url}/raw.png" onerror="globalThis.injected=true">`);
    const rendererRequests: string[] = []; page.on('request', request => rendererRequests.push(request.url()));
    await expect(page.locator('.markdown-message h2')).toHaveText('Verified report');
    await expect(page.locator('.markdown-message')).toContainText('<img src=');
    await expect(page.locator('.markdown-message img')).toHaveCount(0);
    expect(requests).toHaveLength(0);
    let confirm = await load(page);
    await expect(confirm.getByLabel('Destination URL')).toHaveText(url);
    expect(requests).toHaveLength(0);
    await confirm.getByRole('button', { name: 'Cancel', exact: true }).click();
    expect(requests).toHaveLength(0);
    confirm = await load(page);
    await confirm.press('Escape');
    await expect(page.getByRole('button', { name: /^Load image from/ })).toBeFocused();
    expect(requests).toHaveLength(0);
    confirm = await load(page);
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2));
    await expect(confirm.getByRole('button', { name: 'Load image', exact: true })).toBeInViewport();
    await app.evaluate(async ({ BrowserWindow }, destination) => { const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage(); process.getBuiltinModule('fs').writeFileSync(destination, image.toPNG()); }, path.join(evidence, 'url-review-200.png'));
    await confirm.getByRole('button', { name: 'Load image', exact: true }).evaluate(button => { (button as HTMLButtonElement).click(); (button as HTMLButtonElement).click(); });
    const image = page.getByRole('img', { name: 'Result image' });
    await expect(image).toBeVisible();
    await expect.poll(() => image.evaluate(image => (image as HTMLImageElement).naturalWidth)).toBe(1);
    expect(requests).toEqual([{ path: '/image.png?label=preview', cookie: undefined, authorization: undefined, referer: undefined }]);
    const accepted = await app.evaluate(() => (globalThis as typeof globalThis & { reviews: { id: string; kind: 'image' }[] }).reviews.at(-1)!);
    await expect(page.evaluate(request => window.intercat.confirmExternal(request), accepted)).rejects.toThrow();
    await app.evaluate(() => { (globalThis as typeof globalThis & { externalState: { sequence: number } }).externalState.sequence++; });
    await page.waitForTimeout(250);
    expect(requests).toHaveLength(1);
    expect(rendererRequests.filter(url => /^https?:/.test(url))).toEqual([]);
    expect(await page.evaluate(() => (globalThis as { injected?: boolean }).injected)).toBeUndefined();
    await writeFile(path.join(evidence, 'image-network.json'), JSON.stringify({ requests, rendererRequests, cancelledWithoutRequest: true, replayRejected: true }, null, 2));
  } finally { await http.close(); }
});

test('secret URLs and redirects never reach their destination', async () => {
  const paths: string[] = [];
  let target = '';
  const http = await server((request, response) => { paths.push(request.url!); response.writeHead(302, { Location: target }); response.end(); });
  target = `${http.url}/destination`;
  try {
    const token = 'ghp_' + 'A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H';
    let page = await report(`![Secret URL](${http.url}/secret?token=${encodeURIComponent(token)})`);
    await page.getByRole('button', { name: /^Load image from/ }).click();
    await expect(page.getByRole('alert')).toContainText('may contain a secret');
    expect(paths).toEqual([]);
    page = await report(`![Redirect](${http.url}/redirect)`);
    await (await load(page)).getByRole('button', { name: 'Load image', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('Redirects are not followed');
    expect(paths).toEqual(['/redirect']);
    await writeFile(path.join(evidence, 'blocked-network.json'), JSON.stringify({ paths, secretBlocked: true, redirectBlocked: true }));
  } finally { await http.close(); }
});

test('SVG remains an inactive image and external links require their own review', async () => {
  const paths: string[] = [];
  let address = '';
  const http = await server((request, response) => {
    paths.push(request.url!);
    response.writeHead(200, { 'Content-Type': 'image/svg+xml' });
    response.end(`<svg xmlns="http://www.w3.org/2000/svg" width="200" height="80" onload="fetch('${address}/onload')"><script>fetch('${address}/script')</script><image href="${address}/embedded"/><rect width="200" height="80" fill="#334466"/><text x="10" y="40" fill="white">Preview</text></svg>`);
  }); address = http.url;
  try {
    const url = `${http.url}/documentation?label=preview`;
    const page = await report(`[![SVG preview](${http.url}/preview.svg)](${url})\n\n[Blocked](javascript:alert%281%29)`);
    await (await load(page)).getByRole('button', { name: 'Load image', exact: true }).click();
    await expect(page.getByRole('img', { name: 'SVG preview' })).toBeVisible();
    await page.waitForTimeout(300);
    expect(paths).toEqual(['/preview.svg']);
    await page.getByRole('button', { name: 'Open SVG preview', exact: true }).click();
    const confirm = page.getByRole('dialog', { name: 'Confirm link URL' });
    await expect(confirm.getByLabel('Destination URL')).toHaveText(url);
    expect(await app.evaluate(() => (globalThis as typeof globalThis & { opened: string[] }).opened)).toEqual([]);
    await confirm.getByRole('button', { name: 'Open link', exact: true }).click();
    expect(await app.evaluate(() => (globalThis as typeof globalThis & { opened: string[] }).opened)).toEqual([url]);
    await expect(page.locator('.markdown-message a')).toHaveCount(0);
    await expect(page.getByRole('button', { name: 'Blocked', exact: true })).toHaveCount(0);
    await writeFile(path.join(evidence, 'svg-and-link.json'), JSON.stringify({ paths, opened: [url], inactiveSVG: true }));
  } finally { await http.close(); }
});

test('a long destination remains inspectable and cancellable at 200% zoom without a request', async () => {
  let requests = 0;
  const http = await server((_request, response) => { requests++; response.end(pixel); });
  try {
    const url = `${http.url}/long.png?label=${'x'.repeat(7000)}#end`;
    const page = await report(`![Long destination](${url})`);
    await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(2));
    const confirm = await load(page);
    const destination = confirm.getByLabel('Destination URL');
    await expect(destination).toHaveText(url);
    expect(await destination.evaluate(element => element.scrollHeight > element.clientHeight)).toBe(true);
    await destination.focus();
    await destination.press('End');
    await expect.poll(() => destination.evaluate(element => element.scrollTop)).toBeGreaterThan(0);
    await expect(confirm.getByRole('button', { name: 'Cancel', exact: true })).toBeInViewport();
    await expect(confirm.getByRole('button', { name: 'Load image', exact: true })).toBeInViewport();
    await app.evaluate(async ({ BrowserWindow }, destination) => { const image = await BrowserWindow.getAllWindows()[0].webContents.capturePage(); process.getBuiltinModule('fs').writeFileSync(destination, image.toPNG()); }, path.join(evidence, 'long-url-review-200.png'));
    await confirm.press('Escape');
    await expect(page.getByRole('button', { name: /^Load image from/ })).toBeFocused();
    expect(requests).toBe(0);
    await writeFile(path.join(evidence, 'long-url.json'), JSON.stringify({ length: url.length, requests, scrollable: true, cancelled: true, zoom: 2 }));
  } finally { await http.close(); }
});

test('a Markdown image in the timeline step follows the same confirmation gate', async () => {
  let requests = 0;
  const http = await server((_request, response) => { requests++; response.writeHead(200, { 'Content-Type': 'image/png' }); response.end(pixel); });
  try {
    const text = `![Timeline image](${http.url}/step.png)`;
    const page = await report(text);
    await app.evaluate(({ ipcMain }, text) => {
      const now = Date.now();
      const clip = { id: 'image-step', trackId: 'a', ordinal: 1, sessionId: 'session', turnId: 'turn', category: 'step', label: 'Image report', kind: 'think', start: now - 130000, end: now - 45000, running: false, generationEnd: now - 90000, approximate: false, toolTicks: [] };
      const state = (globalThis as typeof globalThis & { externalState: { sequence: number; workers: { output: string }[]; timeline?: unknown } }).externalState;
      state.workers[0].output = '';
      state.timeline = { summaries: [], clock: { epochMs: now, monoMs: 1000, bootId: 'boot', timelineMs: now } };
      state.sequence++;
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [clip], removed: [], marks: [], removedMarks: [] }));
      ipcMain.removeHandler('intercat:clip-detail');
      ipcMain.handle('intercat:clip-detail', () => ({ clip, items: [{ itemId: 'message', kind: 'agentMessage', revision: 1, status: 'completed', text, outputs: {} }] }));
    }, text);
    const step = page.getByRole('button', { name: 'Open step A:1: Image report', exact: true });
    await step.focus(); await step.press('Enter');
    const details = page.getByLabel('Step details');
    await expect(details.locator('.external-image-label')).toContainText('Timeline image');
    expect(requests).toBe(0);
    const confirm = await load(page);
    expect(requests).toBe(0);
    await confirm.getByRole('button', { name: 'Load image', exact: true }).click();
    await expect(details.getByRole('img', { name: 'Timeline image' })).toBeVisible();
    expect(requests).toBe(1);
    await page.screenshot({ path: path.join(evidence, 'timeline-image.png') });
    await writeFile(path.join(evidence, 'timeline-image.json'), JSON.stringify({ beforeConfirmation: 0, afterConfirmation: requests, passed: true }));
  } finally { await http.close(); }
});

test('a real worker Markdown image is fetched only after the user confirms its URL', async () => {
  test.skip(process.env.INTERCAT_EXTERNAL_NATIVE !== '1', 'One paid dummy Muse turn');
  test.setTimeout(180000);
  let requests = 0;
  const http = await server((_request, response) => { requests++; response.writeHead(200, { 'Content-Type': 'image/png' }); response.end(pixel); });
  try {
    const url = `${http.url}/worker.png`;
    const output = path.join(evidence, 'native');
    await execute('python3', ['tools/verify_worker_markdown.py', '--output', output, '--url', url, '--muse', process.env.INTERCAT_TEST_MUSE_BINARY!]);
    const result = JSON.parse(await readFile(path.join(output, 'result.json'), 'utf8'));
    const page = await report(result.text);
    expect(requests).toBe(0);
    const confirm = await load(page);
    await expect(confirm.getByLabel('Destination URL')).toHaveText(url);
    expect(requests).toBe(0);
    await confirm.getByRole('button', { name: 'Load image', exact: true }).click();
    await expect(page.getByRole('img', { name: 'Review image' })).toBeVisible();
    expect(requests).toBe(1);
    await page.screenshot({ path: path.join(evidence, 'native-worker-image.png') });
    await writeFile(path.join(output, 'ui.json'), JSON.stringify({ requests, confirmedURL: url, passed: true }));
  } finally { await http.close(); }
});
