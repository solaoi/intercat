import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { randomUUID } from 'node:crypto';
import path from 'node:path';
const execute = promisify(execFile);

test('Keychain save is preflighted and an unapplied saved value can be removed after restart', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native Keychain test, no model turns');
  test.setTimeout(180000);
  const data = await mkdtemp('/private/tmp/ic-secret-recovery-');
  const source = path.join(data, 'source'); await mkdir(source);
  const output = path.resolve('research/verify/secret-recovery-20261104/native', randomUUID()); await mkdir(output, { recursive: true });
  const dummy = `dummy-unapplied-keychain-secret-${randomUUID()}`;
  await writeFile(path.join(source, 'fixture.txt'), 'Local fixture.\n');
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  await git(['init', '-q', '-b', 'main']); await git(['add', '.']);
  await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture']);
  let app: ElectronApplication | undefined; let pid: number | undefined; let projectId: string | undefined;
  const launch = async () => {
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    const page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 30000 });
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    return page;
  };
  const shutdown = async () => {
    if (app) await closeWindow(app); app = undefined;
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return false; } catch { return true; } }, { timeout: 15000 }).toBe(true);
      pid = undefined;
    }
  };
  try {
    let page = await launch();
    await app!.evaluate(({ dialog }, source) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [source] }); }, source);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).projectSecrets?.ready).toBe(true);
    projectId = (await page.evaluate(() => window.intercat.getStatus())).projectSecrets!.projectId;
    const seeded = await app!.evaluate(async (_electron, input) => {
      const load = process.getBuiltinModule('module').createRequire(input.main);
      const { RpcClient } = load(input.rpc);
      const { uiToken, operationId } = load(input.daemon);
      const { fingerprintKey, fingerprintStore, synchronizeFingerprints, acceptCatalog } = load(input.secrets);
      const { projectRegistration, changeProjectSecret, projectSecretStore } = load(input.project);
      const { AsyncEntry } = load('@napi-rs/keyring');
      const client = await RpcClient.connect(input.socket, await uiToken());
      try {
        const receipt = await fingerprintStore(input.directory); await synchronizeFingerprints(client, receipt);
        const key = await fingerprintKey();
        for (let revision = 1; revision <= 9; revision++) {
          const entries = revision === 9 ? [] : Array.from({ length: 32 }, (_, index) => ({ name: `TOKEN_${index.toString().padStart(2, '0')}`, value: `dummy-history-${revision}-${index}` }));
          await receipt.run(async () => {
            const before = await receipt.read();
            const current = acceptCatalog(await client.request('secret.project.prepare', projectRegistration({ projectId: input.id, revision, entries }, operationId(), key)), before);
            await receipt.publish(before, current); await client.request('secret.confirm', current.catalog);
          });
        }
        const entry = new AsyncEntry('Intercat', `project-secrets.${input.id}`);
        await entry.setPassword(JSON.stringify({ projectId: input.id, revision: 9, entries: [] }));
        await client.request('secret.project.activate', { projectId: input.id, revision: 9, entries: [] });
        let rejected = false;
        try { await changeProjectSecret(client, { opId: operationId(), fingerprintOpId: operationId(), name: 'EXTRA', value: input.dummy, expectedRevision: 9 }); }
        catch (error) { rejected = (error as { code?: number }).code === -32002; }
        const unchanged = await projectSecretStore(input.id).load();
        const before = await client.request('system.status');
        // Reproduce a value already saved by the earlier Keychain-first flow.
        await entry.setPassword(JSON.stringify({ projectId: input.id, revision: 10, entries: [{ name: 'EXTRA', value: input.dummy }] }));
        return { preflightRejected: rejected, savedBundleUnchanged: unchanged.revision === 9 && unchanged.entries.length === 0, oldConfigurationStillReady: before.projectSecrets.ready };
      } finally { client.close(); }
    }, { main: path.resolve('electron/main.mjs'), rpc: path.resolve('electron/rpc-client.mjs'), daemon: path.resolve('electron/daemon.mjs'), secrets: path.resolve('electron/secrets.mjs'), project: path.resolve('electron/project-secrets.mjs'), directory: path.join(data, 'daemon'), socket: path.join(data, 'daemon/run/rpc.sock'), id: projectId, dummy });
    expect(seeded).toEqual({ preflightRejected: true, savedBundleUnchanged: true, oldConfigurationStillReady: true });
    await shutdown(); page = await launch();
    const held = await page.evaluate(() => window.intercat.getStatus());
    expect(held.projectSecrets?.ready).toBe(false);
    expect(held.projectSecretIssue).toBeTruthy();
    expect(JSON.stringify(held).includes(dummy)).toBe(false);
    await page.getByRole('button', { name: 'Review saved secrets', exact: true }).click();
    const settings = page.getByRole('region', { name: 'Project secrets', exact: true });
    await expect(settings).toContainText('EXTRA');
    await settings.getByRole('button', { name: 'Apply saved values', exact: true }).click();
    await expect(settings.getByRole('button', { name: 'Remove EXTRA', exact: true })).toBeEnabled();
    await page.screenshot({ path: path.join(output, 'unapplied.png') });
    await settings.getByRole('button', { name: 'Remove EXTRA', exact: true }).click();
    await expect(settings.getByRole('button', { name: 'Apply saved values', exact: true })).toHaveCount(0);
    await expect(settings).toContainText('No project secrets saved.');
    const recovered = await page.evaluate(() => window.intercat.getStatus());
    expect(recovered.projectSecrets).toMatchObject({ ready: true, revision: 11, names: [] });
    expect(recovered.projectSecretIssue).toBeNull();
    const saved = await page.evaluate(() => window.intercat.getProjectSecretSettings());
    expect(saved.savedNames).toEqual([]); expect(saved.savedRevision).toBe(11);
    expect((await page.content()).includes(dummy)).toBe(false);
    await page.screenshot({ path: path.join(output, 'recovered.png') });
    await writeFile(path.join(output, 'result.json'), JSON.stringify({ ...seeded, connectedWhileHeld: true, recovered, saved, modelTurns: 0 }, null, 2));
  } finally {
    if (app && projectId) await app.evaluate(async (_electron, input) => {
      const load = process.getBuiltinModule('module').createRequire(input.main);
      const { AsyncEntry } = load('@napi-rs/keyring');
      await new AsyncEntry('Intercat', `project-secrets.${input.id}`).deletePassword();
    }, { main: path.resolve('electron/main.mjs'), id: projectId });
    await shutdown();
    const database = path.join(data, 'daemon/events.sqlite');
    const archive = path.join(output, 'events.sqlite');
    await execute('/usr/bin/python3', ['-c', 'import sqlite3,sys; source=sqlite3.connect(sys.argv[1]); target=sqlite3.connect(sys.argv[2]); source.backup(target); target.close(); source.close()', database, archive]);
    const bytes = await readFile(archive); expect(bytes.includes(Buffer.from(dummy))).toBe(false);
    const counts = await execute('/usr/bin/python3', ['-c', "import sqlite3,json,sys; d=sqlite3.connect(sys.argv[1]); print(json.dumps({'modelTurns':d.execute(\"select count(*) from operations where kind='turn.start'\").fetchone()[0], 'historyFingerprints':d.execute('select count(*) from project_fingerprints').fetchone()[0], 'sealed':bool(d.execute('select revision=sealed_revision and digest=sealed_digest from secret_catalog').fetchone()[0])}))", archive]);
    await writeFile(path.join(output, 'storage.json'), counts.stdout);
    expect(JSON.parse(counts.stdout)).toEqual({ modelTurns: 0, historyFingerprints: 256, sealed: true });
    await rm(data, { recursive: true, force: true });
  }
});
