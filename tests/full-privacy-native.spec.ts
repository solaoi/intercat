import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';

test('native Full settings and redactions use only the local Editor and survive restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1' || !process.env.INTERCAT_TEST_EDITOR_MODEL, 'Opt-in local Editor and native Keychain');
  test.setTimeout(240_000);
  const directory = await mkdtemp('/private/tmp/ic-full-privacy-native-');
  const root = process.cwd();
  const output = path.join(root, 'research/verify/m2-full-privacy-20261107');
  let app: Awaited<ReturnType<typeof electron.launch>> | undefined;
  let daemon: number | undefined;
  const launch = async () => {
    app = await electron.launch({ args: ['.', '--user-data-dir=' + directory] });
    const page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    daemon = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate((_electron, modulePath) => {
      const load = process.getBuiltinModule('module').createRequire(modulePath);
      const { RpcClient } = load(modulePath);
      const original = RpcClient.prototype.request;
      const errors: string[] = [];
      Object.assign(globalThis, { nativePrivacyErrors: errors });
      RpcClient.prototype.request = function (this: unknown, method: string, params: unknown) {
        const result = original.call(this, method, params);
        return method === 'privacy.preview' ? result.catch((error: Error) => { errors.push(error.message); throw error; }) : result;
      };
    }, path.join(root, 'electron/rpc-client.mjs'));
    return page;
  };
  try {
    await mkdir(output, { recursive: true });
    let page = await launch();
    const capture = async (filename: string) => {
      await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
      const pixels = await app!.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(path.join(output, filename), Buffer.from(pixels, 'base64'));
    };
    await configureEditor(app!, directory);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).editor?.modelLoaded, { timeout: 90_000 }).toBe(true);
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    const setting = page.getByRole('region', { name: 'Privacy gate', exact: true });
    await expect(setting.getByRole('combobox')).toHaveValue('basic');
    await setting.getByRole('combobox').selectOption('full');
    await expect(setting.getByRole('combobox')).toHaveValue('full');
    await capture('native-settings.png');
    await page.getByRole('dialog', { name: 'Settings · App' }).getByRole('button', { name: 'Close settings', exact: true }).click();
    const dummy = 'My name is Avery Morgan. パスワードは「小舟は夕方に帰る」です。 Ship my package to 123 Example Street, San Francisco, CA 94105.';
    const word = await page.evaluate(async text => window.intercat.stageUtterance({ opId: await window.intercat.createOperationId(), text, target: { mode: 'none' }, selectedTrackId: null, attachmentIds: [] }), dummy);
    const requestButton = () => page.locator(`[data-utterance-id="${word.id}"]`).first();
    await expect(requestButton()).toHaveAttribute('title', /<redacted:name#/, { timeout: 60_000 });
    await requestButton().click();
    let dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    for (const category of ['name', 'password', 'address']) await expect(dialog.getByRole('button', { name: `Show original ${category} value` })).toBeVisible({ timeout: 60_000 });
    for (const [category, original] of [['name', 'Avery Morgan'], ['password', '小舟は夕方に帰る'], ['address', '123 Example Street, San Francisco, CA 94105']]) {
      await dialog.getByRole('button', { name: `Show original ${category} value` }).focus();
      await expect(page.getByRole('tooltip')).toHaveText(original);
      await page.keyboard.press('Escape');
    }
    const before = (await dialog.locator('.request-original .privacy-content').textContent())!;
    for (const zoom of [1, 2]) {
      await app!.evaluate(({ BrowserWindow }, zoom) => { const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom); }, zoom);
      await dialog.getByRole('button', { name: 'Copy masked text', exact: true }).scrollIntoViewIfNeeded();
      await expect(dialog.getByRole('button', { name: 'Copy masked text', exact: true })).toBeInViewport();
      const bounds = await dialog.evaluate(element => { const r = element.getBoundingClientRect(); return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: innerWidth, height: innerHeight }; });
      expect(bounds.left).toBeGreaterThanOrEqual(0); expect(bounds.top).toBeGreaterThanOrEqual(0);
      expect(bounds.right).toBeLessThanOrEqual(bounds.width); expect(bounds.bottom).toBeLessThanOrEqual(bounds.height);
      await capture(`native-full-${zoom * 100}.png`);
    }
    expect((await page.evaluate(() => window.intercat.getStatus())).workers).toEqual([]);
    await expect(page.getByRole('heading', { level: 1 })).not.toHaveText('Privacy check unavailable');
    const firstErrors = await app!.evaluate(() => (globalThis as any).nativePrivacyErrors);
    await writeFile(path.join(output, 'native-first-errors.json'), JSON.stringify(firstErrors));
    expect(firstErrors).toEqual([]);
    await closeWindow(app!); app = undefined;
    if (daemon) {
      process.kill(daemon, 'SIGTERM');
      await expect.poll(() => { try { process.kill(daemon!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    page = await launch();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).editor?.modelLoaded, { timeout: 90_000 }).toBe(true);
    expect((await page.evaluate(() => window.intercat.getStatus())).privacy?.mode).toBe('full');
    await expect(requestButton()).toHaveAttribute('title', /<redacted:name#/, { timeout: 60_000 });
    await requestButton().click();
    dialog = page.getByRole('dialog', { name: 'Request', exact: true });
    await expect(dialog.locator('.request-original .privacy-content')).toHaveText(before, { timeout: 60_000 });
    const status = await page.evaluate(() => window.intercat.getStatus());
    expect(status.workers).toEqual([]);
    await writeFile(path.join(output, 'native-status.json'), JSON.stringify(status, null, 2));
    const errors = await app!.evaluate(() => (globalThis as any).nativePrivacyErrors as string[]);
    // A restored Full preference can be rendered before its model has loaded.
    // The ready-generation refresh above must replace that held preview.
    expect(errors.filter(error => error !== 'Full privacy is waiting for the local Editor.')).toEqual([]);
  } finally {
    if (app) await app.evaluate(() => (globalThis as any).nativePrivacyErrors).then(errors => writeFile(path.join(output, 'native-errors.json'), JSON.stringify(errors)), () => {});
    if (app) await closeWindow(app);
    if (daemon) {
      try { process.kill(daemon, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(daemon!, 0); return true; } catch { return false; } }, { timeout: 15_000 }).toBe(false);
    }
    await rm(directory, { recursive: true, force: true });
  }
});
