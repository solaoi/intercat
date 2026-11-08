import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, stat, mkdir, writeFile, readFile, copyFile } from 'node:fs/promises';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';

test('Keychain keys survive daemon restart and restore the saved draft', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native Keychain integration');
  test.setTimeout(90000);
  const data = await mkdtemp('/private/tmp/ic-ui-');
  let pid: number | undefined;
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 30000 });
    const status = await page.evaluate(() => window.intercat.getStatus());
    pid = status.daemonPid;
    expect(pid).toBeGreaterThan(0);
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await page.getByLabel('Stops', { exact: true }).selectOption('every-approval');
    await mkdir('research/verify/policy-20261104', { recursive: true });
    await page.screenshot({ path: 'research/verify/policy-20261104/settings.png' });
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).stops).toBe('every-approval');
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Dummy RPC fixture. Keep it local.');
    await page.getByRole('button', { name: 'Put' }).click();
    await expect(page.locator('.draft-strip')).toContainText('Dummy RPC fixture. Keep it local.');
    expect((await stat(path.join(data, 'daemon/run/rpc.sock'))).mode & 0o777).toBe(0o600);
    await closeWindow(app);
    process.kill(pid!, 0);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    await expect(page.locator('.draft-strip')).toContainText('Dummy RPC fixture. Keep it local.');
    const restored = await page.evaluate(() => window.intercat.getStatus());
    expect(restored.daemonPid).toBe(pid);
    expect(restored.stops).toBe('every-approval');
    await page.getByRole('button', { name: 'Settings', exact: true }).click();
    await expect(page.getByLabel('Stops', { exact: true })).toHaveValue('every-approval');
    await page.getByRole('button', { name: 'Close settings', exact: true }).click();
    expect(restored.utterances?.[0].text).toBe('Dummy RPC fixture. Keep it local.');
    await page.screenshot({ path: 'research/verify/rpc-20261104/connected.png' });
    await closeWindow(app);
    const firstPid=pid;
    process.kill(pid!,'SIGTERM');
    await expect.poll(() => { try { process.kill(pid!,0);return false; } catch { return true; } }).toBe(true);
    app=await electron.launch({args:['.',`--user-data-dir=${data}`]});
    page=await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
    const restarted=await page.evaluate(()=>window.intercat.getStatus());
    pid=restarted.daemonPid;
    expect(pid).not.toBe(firstPid);
    expect(restarted.utterances?.[0].text).toBe('Dummy RPC fixture. Keep it local.');
    const database=JSON.parse((await promisify(execFile)('python3',['-c',"import sqlite3,json,sys; d=sqlite3.connect(sys.argv[1]); print(json.dumps({'schema':d.execute('pragma user_version').fetchone()[0],'checkBytes':d.execute('select length(check_tag) from secret_key').fetchone()[0]}))",path.join(data,'daemon/events.sqlite')])).stdout);
    expect(database).toEqual({schema:53,checkBytes:32});
    const db=path.join(data,'daemon/events.sqlite');
    const older=path.join(data,'older.sqlite'), latest=path.join(data,'latest.sqlite');
    const sqlite=async(code:string,args:string[])=>promisify(execFile)('python3',['-c',code,...args]);
    await sqlite("import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); d.execute('vacuum into ?', (sys.argv[2],))",[db,older]);
    await sqlite("import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); d.execute(\"insert into msp_positions values('catalog-session','catalog-track',null)\"); d.commit()",[db]);
    const catalog=await app.evaluate(async (_electron,input)=>{
      const load=process.getBuiltinModule('module').createRequire(input.module);
      const {RpcClient}=load(input.rpc);
      const {uiToken}=load(input.daemon);
      const {fingerprintStore,synchronizeFingerprints,registerFingerprint}=load(input.secrets);
      const client=await RpcClient.connect(input.socket,await uiToken());
      try {
        await synchronizeFingerprints(client,await fingerprintStore(input.directory));
        await registerFingerprint(client,{opId:input.opId,trackId:'catalog-track',sessionId:'catalog-session',name:'TOKEN',value:'synthetic catalog fixture'});
        return await client.request('secret.catalog');
      } finally { client.close(); }
    },{module:path.resolve('electron/main.mjs'),rpc:path.resolve('electron/rpc-client.mjs'),daemon:path.resolve('electron/daemon.mjs'),secrets:path.resolve('electron/secrets.mjs'),socket:path.join(data,'daemon/run/rpc.sock'),directory:path.join(data,'daemon'),opId:'01a1a200-0000-7000-8000-000000000123'});
    expect(catalog.current.revision).toBe(1);
    expect(catalog.sealed).toEqual(catalog.current);
    await sqlite("import sqlite3,sys; d=sqlite3.connect(sys.argv[1]); d.execute('vacuum into ?', (sys.argv[2],))",[db,latest]);
    await closeWindow(app);process.kill(pid!,'SIGTERM');
    await expect.poll(()=>{try{process.kill(pid!,0);return false;}catch{return true;}}).toBe(true);
    pid=undefined;
    for(const file of [db,db+'-wal',db+'-shm']) await rm(file,{force:true});
    await copyFile(older,db);
    app=await electron.launch({args:['.',`--user-data-dir=${data}`]});page=await app.firstWindow();
    await expect(page.getByRole('alert')).toContainText('does not match its saved fingerprints');
    const mismatched = await page.evaluate(()=>window.intercat.getStatus());
    expect(mismatched.connected).toBe(false);
    expect(mismatched.fingerprintRecovery).toEqual({ sessions: [{ sessionId: 'catalog-session', trackId: 'catalog-track', reason: 'missing' }], projects: [] });
    await expect(page.getByRole('region', { name: 'Fingerprint recovery review' })).toContainText('catalog-session');
    await mkdir('research/verify/fingerprint-witness-20261104', { recursive: true });
    await page.screenshot({ path: 'research/verify/fingerprint-witness-20261104/rollback.png' });
    expect(await stat(path.join(data,'daemon/run/rpc.sock')).then(()=>true,()=>false)).toBe(false);
    const failures=(await readFile(path.join(data,'daemon/daemon.log'),'utf8')).match(/saved fingerprint receipt does not match/g)?.length;
    await page.evaluate(()=>window.intercat.getStatus());
    expect((await readFile(path.join(data,'daemon/daemon.log'),'utf8')).match(/saved fingerprint receipt does not match/g)?.length).toBe(failures);
    for(const file of [db,db+'-wal',db+'-shm']) await rm(file,{force:true});
    await copyFile(latest,db);
    await page.getByRole('button',{name:'Reconnect',exact:true}).click();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
    pid=(await page.evaluate(()=>window.intercat.getStatus())).daemonPid;
    await expect(page.locator('.draft-strip')).toContainText('Dummy RPC fixture. Keep it local.');
    await mkdir('research/verify/fingerprint-witness-20261104',{recursive:true});
    await writeFile('research/verify/fingerprint-witness-20261104/keychain-result.json',JSON.stringify({firstPid,restartedPid:pid,...database,savedDraftRestored:true,fingerprintRevision:catalog.current.revision,rollbackRejected:true,affected:mismatched.fingerprintRecovery,reconnectedAfterRestoration:true,modelTurns:0},null,2));
  } finally {
    await closeWindow(app);
    if (pid) process.kill(pid, 'SIGTERM');
    await rm(data, { recursive: true, force: true });
  }
});

test('local editor classifies a queued request through the UI and exits with its daemon', async () => {
  test.skip(!process.env.INTERCAT_TEST_EDITOR_MODEL || !process.env.INTERCAT_TEST_EDITOR_PYTHON, 'Opt-in local MLX model');
  test.setTimeout(90000);
  const data = await mkdtemp('/private/tmp/ic-editor-ui-');
  const source = path.join(data, 'source');
  await mkdir(source);
  const git = (args: string[]) => promisify(execFile)('/usr/bin/git', ['-C', source, ...args], { env: { PATH: process.env.PATH, HOME: data, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_NOSYSTEM: '1' } });
  await git(['init', '-b', 'main']);
  await writeFile(path.join(source, 'arithmetic.js'), 'export const add = (a, b) => a + b;\n');
  await git(['add', '.']);
  await git(['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-m', 'arithmetic fixture']);
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  let daemonPid: number | undefined;
  let editorPid: number | undefined;
  let workerPid: number | undefined;
  let workerSession: string | undefined;
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    daemonPid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => {
      const picks = paths.map(file => ({ canceled: false, filePaths: [file] }));
      dialog.showOpenDialog = async () => picks.shift()!;
    }, [source, process.env.INTERCAT_TEST_EDITOR_MODEL!, process.env.INTERCAT_TEST_EDITOR_PYTHON!, ...(process.env.INTERCAT_TEST_MUSE_BINARY ? [process.env.INTERCAT_TEST_MUSE_BINARY] : [])]);
    await page.getByRole('button', { name: 'Choose project folder', exact: true }).click();
    await configureEditor(app, data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
    const state = await page.evaluate(() => window.intercat.getStatus());
    daemonPid = state.daemonPid;
    editorPid = (state.editor as typeof state.editor & { pid: number }).pid;
    expect(state.editor).toMatchObject({ process: true, accepting: true, modelLoaded: true, restart: 0 });
    const request = 'Create a new arithmetic test. Do not start until I explicitly tell you.';
    await page.getByRole('textbox', { name: 'Request draft' }).fill(request);
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.text === request)?.decision?.kind, { timeout: 15000 }).toBe('fork');
    const classified = await page.evaluate(() => window.intercat.getStatus());
    const word = classified.utterances!.find(word => word.text === request)!;
    const wordId = word.id;
    expect(word.decision?.boundary).toBe('when-told');
    expect(word.state).toBe('placed');
    expect(word.decision?.timings?.[0]?.roundTripMs).toBeGreaterThanOrEqual(word.decision!.inferenceMs);
    await writeFile(test.info().outputPath('decision-timing.json'), JSON.stringify({
      timings: word.decision!.timings,
    }, null, 2));
    expect(classified.tracks).toHaveLength(1);
    await expect(page.locator('.request-chip.placed')).toHaveCount(1);
    await expect(page.getByRole('row')).toHaveCount(1);
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workspaces?.[0]?.state).toBe('ready');
    const prepared = (await page.evaluate(() => window.intercat.getStatus())).workspaces![0];
    expect(prepared.base).toBe(classified.project!.folders[0].base);
    expect(await readFile(path.join(prepared.path, 'arithmetic.js'), 'utf8')).toBe('export const add = (a, b) => a + b;\n');
    if (process.env.INTERCAT_TEST_MUSE_BINARY) {
      await page.getByRole('button', { name: 'Connect workers', exact: true }).click();
      await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.phase, { timeout: 20000 }).toBe('ready');
      const worker = (await page.evaluate(() => window.intercat.getStatus())).workers![0];
      expect(worker.sessionId).toBeTruthy();
      workerPid = worker.identity!.pid;
      workerSession = worker.sessionId!;
    }
    await mkdir('research/verify/editor-ui-20261104', { recursive: true });
    await writeFile('research/verify/editor-ui-20261104/classified.json', JSON.stringify(word, null, 2));
    const command = 'Open the commit sheet.';
    await page.getByRole('textbox', { name: 'Request draft' }).fill(command);
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(item => item.text === command)?.decision?.command, { timeout: 15000 }).toBe('commit');
    await expect(page.locator('.request-chip.placed')).toHaveCount(2);
    await page.screenshot({ path: 'research/verify/editor-ui-20261104/ready.png' });
    await page.getByRole('button', { name: `Cut request: ${request}`, exact: true }).click();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(word => word.id === wordId)?.state, { timeout: 5000 }).toBe('cut');
    if (workerSession) {
      await closeWindow(app);
      process.kill(daemonPid!, 'SIGTERM');
      for (const pid of [daemonPid, editorPid, workerPid]) {
        await expect.poll(() => { try { process.kill(pid!, 0); return true; } catch { return false; } }, { timeout: 10000 }).toBe(false);
      }
      app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
      page = await app.firstWindow();
      await expect(page.locator('.status-bar')).toContainText('Daemon connected');
      daemonPid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
      await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).workers?.[0]?.phase, { timeout: 20000 }).toBe('ready');
      await expect(page.locator('.status-bar')).toContainText('Local editor · ready', { timeout: 60000 });
      const resumed = await page.evaluate(() => window.intercat.getStatus());
      expect(resumed.workers![0].sessionId).toBe(workerSession);
      expect(resumed.utterances!.find(word => word.id === wordId)?.state).toBe('cut');
      workerPid = resumed.workers![0].identity!.pid;
      editorPid = (resumed.editor as typeof resumed.editor & { pid: number }).pid;
      await page.screenshot({ path: 'research/verify/editor-ui-20261104/resumed-worker.png' });
      process.kill(workerPid, 'SIGKILL');
      await expect.poll(async () => {
        const worker = (await page.evaluate(() => window.intercat.getStatus())).workers?.[0];
        return worker?.phase === 'ready' && !!worker.identity && worker.identity.pid !== workerPid;
      }, { timeout: 20000 }).toBe(true);
      const retried = (await page.evaluate(() => window.intercat.getStatus())).workers![0];
      expect(retried.sessionId).toBe(workerSession);
      expect(retried.identity!.pid).not.toBe(workerPid);
      workerPid = retried.identity!.pid;

    }
    await promisify(execFile)('/usr/bin/python3', ['-c', 'import sqlite3,sys; source=sqlite3.connect(sys.argv[1]); target=sqlite3.connect(sys.argv[2]); source.backup(target); target.close(); source.close()', path.join(data, 'daemon/events.sqlite'), path.resolve('research/verify/editor-ui-20261104/events.sqlite')]);
  } finally {
    await closeWindow(app);
    if (daemonPid) process.kill(daemonPid, 'SIGTERM');
    if (editorPid) await expect.poll(() => { try { process.kill(editorPid!, 0); return true; } catch { return false; } }).toBe(false);
    if (workerPid) await expect.poll(() => { try { process.kill(workerPid!, 0); return true; } catch { return false; } }).toBe(false);
    await rm(data, { recursive: true, force: true });
  }
});
