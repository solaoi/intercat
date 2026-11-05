import { closeWindow } from './support/close';
import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, readFile, copyFile, rm, stat } from 'node:fs/promises';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { randomUUID } from 'node:crypto';
import path from 'node:path';

const execute = promisify(execFile);
test('saved project fingerprints rebuild after rollback while missing one-time history stays closed', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Native Keychain fixture; no model calls');
  test.setTimeout(150000);
  const data = await mkdtemp('/private/tmp/ic-fingerprint-rebuild-');
  const source = path.join(data, 'source'); await mkdir(source);
  await writeFile(path.join(source, 'fixture.txt'), 'Local fixture only.\n');
  const git = (args: string[]) => execute('/usr/bin/git', ['-C', source, ...args], { env: { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  await git(['init', '-q', '-b', 'main']); await git(['add', '.']);
  await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture']);
  const output = path.resolve('research/verify/fingerprint-rebuild-20261104/native', randomUUID()); await mkdir(output, { recursive: true });
  const dummy = `dummy-project-rebuild-${randomUUID()}`;
  const once = `dummy-unrecoverable-once-${randomUUID()}`;
  const database = path.join(data, 'daemon/events.sqlite');
  const backup = path.join(data, 'before.sqlite');
  const sqlite = (code: string, args: string[]) => execute('/usr/bin/python3', ['-c', code, ...args]);
  let app: ElectronApplication | undefined; let pid: number | undefined; let projectId: string | undefined;
  const launch = async (connected: boolean) => {
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    const page = await app.firstWindow();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).connected).toBe(connected);
    pid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    return page;
  };
  const shutdown = async () => {
    if (app) await closeWindow(app); app = undefined;
    if (pid) {
      try { process.kill(pid, 'SIGTERM'); } catch { /* Already stopped. */ }
      await expect.poll(() => { try { process.kill(pid!, 0); return false; } catch { return true; } }).toBe(true);
      pid = undefined;
    }
  };
  try {
    let page = await launch(true);
    await app!.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, source);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).projectSecrets?.ready).toBe(true);
    projectId = (await page.evaluate(() => window.intercat.getStatus())).projectSecrets!.projectId;
    await sqlite('import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); d.execute("vacuum into ?",(sys.argv[2],))', [database, backup]);
    await sqlite("import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); d.execute(\"insert into msp_positions values('missing-once-session','missing-once-track',null)\"); d.commit()", [database]);
    const input = { main: path.resolve('electron/main.mjs'), rpc: path.resolve('electron/rpc-client.mjs'), daemon: path.resolve('electron/daemon.mjs'), secrets: path.resolve('electron/secrets.mjs'), project: path.resolve('electron/project-secrets.mjs'), directory: path.join(data, 'daemon'), socket: path.join(data, 'daemon/run/rpc.sock'), projectId, dummy, once };
    const expectedReceipt = await app!.evaluate(async (_electron, value) => {
      const load = process.getBuiltinModule('module').createRequire(value.main);
      const { RpcClient } = load(value.rpc); const { uiToken, operationId } = load(value.daemon);
      const { fingerprintStore, synchronizeFingerprints, registerFingerprint } = load(value.secrets);
      const { changeProjectSecret } = load(value.project);
      const client = await RpcClient.connect(value.socket, await uiToken());
      const store = await fingerprintStore(value.directory);
      try {
        await synchronizeFingerprints(client, store);
        await changeProjectSecret(client, { opId: operationId(), fingerprintOpId: operationId(), name: 'PROJECT_TOKEN', value: value.dummy, expectedRevision: 0 });
        await registerFingerprint(client, { opId: operationId(), trackId: 'missing-once-track', sessionId: 'missing-once-session', name: 'ONCE_TOKEN', value: value.once });
        return await store.read();
      } finally { client.close(); }
    }, input);
    await shutdown();
    for (const file of [database, database + '-wal', database + '-shm']) await rm(file, { force: true });
    await copyFile(backup, database);
    page = await launch(false);
    const review = page.getByRole('region', { name: 'Fingerprint recovery review' });
    await expect(review).toContainText('missing-once-session', { timeout: 30000 });
    const savedBundle = await app!.evaluate(async (_electron, value) => {
      const load = process.getBuiltinModule('module').createRequire(value.main);
      const entry = new (load('@napi-rs/keyring').AsyncEntry)('Intercat', `project-secrets.${value.projectId}`);
      const saved = await entry.getPassword(); const changed = JSON.parse(saved);
      changed.entries[0].value = 'wrong-dummy-value'; await entry.setPassword(JSON.stringify(changed)); return saved;
    }, input);
    await review.getByRole('button', { name: 'Rebuild saved fingerprints', exact: true }).click();
    await expect(review.getByRole('alert')).toContainText('cannot reconstruct');
    const unchanged = await sqlite("import sqlite3,json,sys; d=sqlite3.connect(sys.argv[1]); print(json.dumps({'rows':d.execute('select count(*) from project_fingerprints').fetchone()[0], 'pending':d.execute(\"select count(*) from settings where key='fingerprint-recovery'\").fetchone()[0]}))", [database]);
    expect(JSON.parse(unchanged.stdout)).toEqual({ rows: 0, pending: 0 });
    await app!.evaluate(async (_electron, value) => {
      const load = process.getBuiltinModule('module').createRequire(value.main);
      await new (load('@napi-rs/keyring').AsyncEntry)('Intercat', `project-secrets.${value.id}`).setPassword(value.saved);
    }, { main: input.main, id: projectId, saved: savedBundle });
    await review.getByRole('button', { name: 'Rebuild saved fingerprints', exact: true }).click();
    await expect(review).toContainText('Saved fingerprints rebuilt for 1 project');
    await expect(review).toBeFocused();
    await expect(review).toContainText('missing-once-session');
    await expect(review.getByRole('button', { name: 'Rebuild saved fingerprints', exact: true })).toHaveCount(0);
    expect(await stat(path.join(data, 'daemon/run/rpc.sock')).then(() => true, () => false)).toBe(false);
    const held = await page.evaluate(() => window.intercat.getStatus());
    expect(held.connected).toBe(false); expect(held.fingerprintRecovery?.historyReviewRequired).toBe(true);
    expect(JSON.stringify(held)).not.toContain(dummy); expect(JSON.stringify(held)).not.toContain(once);
    await page.screenshot({ path: path.join(output, 'rebuilt.png') });
    const receipt = await app!.evaluate(async (_electron, value) => {
      const load = process.getBuiltinModule('module').createRequire(value.main);
      return (await load(value.secrets).fingerprintStore(value.directory)).read();
    }, input);
    expect(receipt).toEqual(expectedReceipt);
    await shutdown(); page = await launch(false);
    await expect(page.getByRole('region', { name: 'Fingerprint recovery review' })).toContainText('Saved fingerprints rebuilt for 1 project');
    const storage = await sqlite("import sqlite3,json,sys; d=sqlite3.connect(sys.argv[1]); print(json.dumps({'projectFingerprints':d.execute('select count(*) from project_fingerprints').fetchone()[0], 'oneTimeFingerprints':d.execute('select count(*) from secret_fingerprints').fetchone()[0], 'rebuilds':d.execute(\"select count(*) from operations where kind='fingerprints.rebuild'\").fetchone()[0], 'modelTurns':d.execute(\"select count(*) from operations where kind='turn.start'\").fetchone()[0]}))", [database]);
    expect(JSON.parse(storage.stdout)).toEqual({ projectFingerprints: 1, oneTimeFingerprints: 0, rebuilds: 1, modelTurns: 0 });
    await writeFile(path.join(output, 'result.json'), JSON.stringify({ held, storage: JSON.parse(storage.stdout), receiptUnchanged: true, sourceUnchanged: (await readFile(path.join(source, 'fixture.txt'), 'utf8')) === 'Local fixture only.\n' }, null, 2));
  } finally {
    if (app && projectId) await app.evaluate(async (_electron, value) => {
      const load = process.getBuiltinModule('module').createRequire(value.main);
      await new (load('@napi-rs/keyring').AsyncEntry)('Intercat', `project-secrets.${value.id}`).deletePassword();
    }, { main: path.resolve('electron/main.mjs'), id: projectId });
    await shutdown();
    await sqlite('import sqlite3,sys; s=sqlite3.connect(sys.argv[1]); t=sqlite3.connect(sys.argv[2]); s.backup(t); t.close(); s.close()', [database, path.join(output, 'events.sqlite')]);
    const bytes = await readFile(path.join(output, 'events.sqlite')); expect(bytes.includes(Buffer.from(dummy))).toBe(false); expect(bytes.includes(Buffer.from(once))).toBe(false);
    await rm(data, { recursive: true, force: true });
  }
});
