import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { mkdir, mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { promisify } from 'node:util';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

// Real renderer/preload, shared production comparison IPC, RpcClient and daemon.
// Only shell status/integration forwarding and draft storage are replaced. No
// Keychain, configured worker, model call or user repository is involved.
const execute = promisify(execFile);
let root: string, fixture: { data: string; source: string; request: { parent: unknown; child: unknown }; workspaces: string[]; receipt: unknown };
let daemon: ChildProcess | undefined, app: ElectronApplication | undefined;
let token: string, receipt: unknown, startupError = '';
const evidence = path.resolve('research/verify/m4-comparison-rpc-20261110-01');
const fixtureTest = 'reparent_store::tests::comparisons::export_electron_comparison_fixture';
test.describe.configure({ mode: 'serial' });

async function startDaemon() {
  expect(daemon).toBeUndefined();
  startupError = '';
  token = randomBytes(32).toString('hex');
  daemon = spawn(path.resolve(process.env.CARGO_TARGET_DIR ?? 'target', 'debug/intercatd'), ['serve', fixture.data, path.resolve('services/editor/src')], {
    stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env, INTERCAT_GITLEAKS: path.resolve('.tools/gitleaks-8.30.1/gitleaks'), INTERCAT_GIT_LFS: path.resolve('.tools/git-lfs-3.8.1/git-lfs') },
  });
  daemon.stderr!.on('data', (chunk: Buffer) => { startupError = (startupError + chunk.toString()).slice(-4096); });
  const lines = createInterface({ input: daemon.stdout! });
  const handshake = (async () => {
    for await (const line of lines) {
      const hello = JSON.parse(line);
      if (!hello.fingerprints) throw new Error('Missing own fixture bootstrap');
      receipt = hello.fingerprints;
      daemon!.stdin!.end(JSON.stringify(hello.fingerprints.catalog) + '\n');
      lines.close(); return;
    }
    throw new Error(`Own daemon bootstrap failed: ${startupError}`);
  })();
  daemon.stdin!.write(JSON.stringify({ token, secretKey: Buffer.alloc(32, 7).toString('hex'), receipt }) + '\n');
  await handshake;
  await expect.poll(async () => {
    if (daemon!.exitCode !== null) throw new Error(`Own daemon exited: ${startupError}`);
    return stat(path.join(fixture.data, 'run/rpc.sock')).then(value => value.isSocket(), () => false);
  }, { timeout: 15_000 }).toBe(true);
}
async function stopDaemon() {
  if (daemon && daemon.exitCode === null && daemon.signalCode === null) {
    const closed = new Promise(resolve => daemon!.once('exit', resolve)); daemon.kill('SIGTERM'); await closed;
  }
  daemon = undefined;
}
async function launch(zoom: number) {
  app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + path.join(root, `ui-${zoom}`)] });
  await mockDrafts(app);
  await app.evaluate(async ({ ipcMain, BrowserWindow }, input) => {
    const load = process.getBuiltinModule('module').createRequire(input.rpc);
    const { RpcClient } = load(input.rpc);
    const { registerComparison } = load(input.comparison);
    try { await RpcClient.connect(input.socket, '0'.repeat(64)); throw new Error('Wrong own token was accepted'); }
    catch (error) { if ((error as { code?: number }).code !== -32001) throw error; }
    const client = await RpcClient.connect(input.socket, input.token);
    const requests: { method: string; params: unknown }[] = [];
    const request = client.request.bind(client);
    client.request = (method: string, params: unknown) => { requests.push({ method, params }); return request(method, params); };
    const window = BrowserWindow.getAllWindows()[0];
    const authorize = (event: { sender: { id: number } }) => { if (event.sender.id !== window.webContents.id) throw new Error('Wrong own renderer'); };
    for (const channel of ['open', 'control', 'read', 'evidence', 'diff']) ipcMain.removeHandler(`intercat:comparison-${channel}`);
    registerComparison({ ipcMain, authorize, preview: false, connection: async () => client });
    for (const [channel, method] of [['status', 'system.status'], ['integration-command', 'integration.command'], ['timeline-window', 'timeline.window']]) {
      ipcMain.removeHandler(`intercat:${channel}`);
      ipcMain.handle(`intercat:${channel}`, async (event, value) => { authorize(event); const result = await client.request(method, value ?? {}); return channel === 'status' ? { connected: true, ...result } : result; });
    }
    Object.assign(globalThis, { comparisonRpcFixture: { client, requests } });
    window.setSize(1120, 800); window.webContents.setZoomFactor(input.zoom);
  }, { rpc: path.resolve('electron/rpc-client.mjs'), comparison: path.resolve('electron/comparison.mjs'), socket: path.join(fixture.data, 'run/rpc.sock'), token, zoom });
  const page = await app.firstWindow(); await page.reload();
  await expect(page.locator('.status-bar')).toContainText('Daemon connected');
  return page;
}
async function capture(page: Page, name: string) {
  const state = await page.evaluate(() => window.intercat.getStatus());
  await writeFile(path.join(evidence, name + '.json'), JSON.stringify(state, null, 2));
  const png = await app!.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
  await writeFile(path.join(evidence, name + '.png'), Buffer.from(png, 'base64'));
}
test.beforeAll(async () => {
  test.setTimeout(180_000);
  root = await mkdtemp('/private/tmp/ic-own-comparison-rpc-'); await mkdir(evidence, { recursive: true });
  await execute('cargo', ['build', '--offline', '-q', '--bin', 'intercatd']);
  const exported = await execute('cargo', ['test', '--offline', '-q', '-p', 'intercat-daemon', '--lib', fixtureTest, '--', '--ignored', '--exact', '--nocapture']);
  const match = /OWN_COMPARISON_FIXTURE=(.+)/.exec(exported.stdout);
  if (!match) throw new Error('Own comparison fixture was not exported');
  const directory = match[1].trim();
  fixture = JSON.parse(await readFile(path.join(directory, 'fixture.json'), 'utf8'));
  expect(fixture.data).toBe(directory); receipt = fixture.receipt;
  await startDaemon();
});
test.afterEach(async () => { if (app) { await closeWindow(app); app = undefined; } });
test.afterAll(async () => {
  await stopDaemon();
  if (fixture) await rm(fixture.data, { recursive: true, force: true });
  if (root) await rm(root, { recursive: true, force: true });
});

test('actual RPC compares fixed paths, holds/retries, masks output and keeps manual choice', async () => {
  test.setTimeout(90_000);
  const page = await launch(1);
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review', exact: true });
  const choices = sheet.getByRole('radio'); await expect(choices).toHaveCount(2);
  await sheet.getByRole('button', { name: /^Check both paths ·/ }).click();
  const dialog = page.getByRole('dialog', { name: 'Compare saved paths', exact: true }); await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: 'Stop checks', exact: true }).click();
  await expect(dialog.getByRole('button', { name: 'Retry unfinished checks', exact: true })).toBeEnabled();
  await capture(page, 'held-100');
  // Subsequent unsaved edits must not enter either frozen comparison input.
  for (const workspace of fixture.workspaces) await writeFile(path.join(workspace, 'outcome.txt'), 'Own unsaved later work\n');
  await dialog.getByRole('button', { name: 'Retry unfinished checks', exact: true }).click();
  await expect(dialog).toContainText('Only A passes the checks', { timeout: 30_000 });
  for (const output of await dialog.getByText('Check output', { exact: true }).all()) await output.click();
  await expect(dialog).not.toContainText('OwnDummyCompareOnly20261110');
  await expect(dialog.getByRole('heading', { name: /Worker A2 Check failed · exit 1/ })).toBeVisible();
  await dialog.getByRole('button', { name: 'Compare the diffs', exact: true }).click();
  const diff = dialog.getByRole('region', { name: 'Diff for outcome.txt', exact: true });
  await expect(diff.locator('.monaco-diff-editor')).toBeVisible();
  await expect(diff.getByText('parent', { exact: true })).toBeVisible();
  await expect(diff.getByText('child', { exact: true })).toBeVisible();
  await expect(diff).not.toContainText('Own unsaved later work');
  const saved = (await page.evaluate(() => window.intercat.getStatus())).comparisons![0];
  expect(saved.request.parent).toEqual(fixture.request.parent); expect(saved.request.child).toEqual(fixture.request.child);
  expect(saved.sides.map(side => side.outcome?.check?.code)).toEqual([0, 1]);
  expect(saved.attempt).toBe(2); expect(JSON.stringify(saved)).not.toContain('OwnDummyCompareOnly20261110');
  const invalid = await page.evaluate(async id => { try { await window.intercat.comparisonDiff({ id, basis: 'paths', path: '../own-denied' }); return ''; } catch (error) { return String(error); } }, saved.id);
  expect(invalid).toContain('Invalid saved path comparison');
  const rejected = await app!.evaluate(async (_electron, id) => {
    const client = (globalThis as any).comparisonRpcFixture.client;
    try { await client.request('comparison.diff', { id, basis: 'paths', path: '../own-denied' }); return null; }
    catch (error) { return { code: (error as { code?: number }).code, message: String(error) }; }
  }, saved.id);
  expect(rejected?.code).toBe(-32005);
  expect(rejected?.message).toContain("This file is not in the saved paths' diff.");
  await capture(page, 'diff-100');
  await dialog.getByRole('button', { name: 'Back to path choices', exact: true }).click();
  for (const choice of await choices.all()) await expect(choice).not.toBeChecked();
  const state = await page.evaluate(() => window.intercat.getStatus());
  expect(state.integrations![0].pathReview.choices).toEqual({});
  await closeWindow(app!); app = undefined; await stopDaemon();
});

test('saved real comparison survives daemon restart at 200 percent without another Check', async () => {
  test.setTimeout(90_000);
  await startDaemon(); const page = await launch(2);
  await page.getByRole('button', { name: 'Commit', exact: true }).click();
  const sheet = page.getByRole('complementary', { name: 'Commit review', exact: true });
  await sheet.getByRole('button', { name: /^Compare saved results ·/ }).click();
  const dialog = page.getByRole('dialog', { name: 'Compare saved paths', exact: true });
  await expect(dialog).toContainText('Only A passes the checks');
  await dialog.getByRole('button', { name: 'Compare the diffs', exact: true }).click();
  await expect(dialog.locator('.monaco-diff-editor')).toBeVisible();
  await dialog.getByText('child', { exact: true }).scrollIntoViewIfNeeded();
  await expect(dialog.getByText('child', { exact: true })).toBeVisible();
  const back = dialog.getByRole('button', { name: 'Back to path choices', exact: true });
  await back.focus(); await expect(back).toBeFocused();
  expect(await back.evaluate(element => { const rect = element.getBoundingClientRect(); return element.contains(document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2)); })).toBe(true);
  await capture(page, 'restored-200');
  const requests = await app!.evaluate(() => (globalThis as any).comparisonRpcFixture.requests);
  expect(requests.some((request: { method: string }) => ['comparison.open', 'comparison.control'].includes(request.method))).toBe(false);
  expect(requests.filter((request: { method: string }) => request.method === 'comparison.diff')).toHaveLength(1);
  await dialog.getByRole('button', { name: 'Back to path choices', exact: true }).click();
  for (const choice of await sheet.getByRole('radio').all()) await expect(choice).not.toBeChecked();
});
