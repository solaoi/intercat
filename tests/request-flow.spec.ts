import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import path from 'node:path';

test('a held request survives closing the UI and a chosen destination is re-read', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon integration');
  test.setTimeout(240_000);
  const data = await mkdtemp('/private/tmp/ic-request-ui-');
  const moduleRoot = path.join(data, 'fixture');
  const packageRoot = path.join(moduleRoot, 'intercat_editor');
  const model = path.join(data, 'model');
  await mkdir(packageRoot, { recursive: true });
  await mkdir(model);
  await writeFile(path.join(packageRoot, '__init__.py'), '');
  // A deterministic local HTTP fixture tests the UI/RPC state machine, not model quality.
  await writeFile(path.join(packageRoot, 'server.py'), `import json,math
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def respond(self,value):
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self): self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  if self.path=='/tokens':self.respond({'tokens':10});return
  context=json.loads(request['tail']);selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()): continue
   labels=stage['candidates'];choice=(' A' if context['target']['mode']=='lock' else ' F') if stage['name']=='decision' else ' D'
   selected[stage['name']]=choice;n=len(labels);probabilities={x:1 if n==1 else .9 if x==choice else .1/(n-1) for x in labels}
   stages[stage['name']]={'selected':choice,'logprobs':{x:math.log(probabilities[x])*request['temperature'] for x in labels},'probabilities':probabilities}
  self.respond({'cachePrefix':False,'temperature':request['temperature'],'stages':stages,'inferenceMs':10})
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
`);
  // The launcher always supplies the actual source path. Use a fixture Python wrapper to select our module.
  const python = path.join(data, 'python');
  await writeFile(python, `#!/bin/sh\nPYTHONPATH='${moduleRoot}' exec /usr/bin/python3 "$@"\n`, { mode: 0o700 });
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`, ...(process.env.INTERCAT_TEST_DAEMON ? ['--preview'] : [])] });
  let daemonPid: number | undefined;
  const editorStartup: unknown[] = [];
  try {
    if (process.env.INTERCAT_TEST_DAEMON) {
      await app.evaluate(async (_electron, paths) => {
        const load = process.getBuiltinModule('module').createRequire(paths.module);
        const { connectDaemon, uiToken } = load(paths.module);
        const client = await connectDaemon(paths.binary, paths.directory, await uiToken(), paths.editor, paths.gitleaks, paths.gitLfs);
        client.close();
      }, { module: path.resolve('electron/daemon.mjs'), binary: process.env.INTERCAT_TEST_DAEMON, directory: path.join(data, 'daemon'), editor: path.resolve('services/editor/src'), gitleaks: path.resolve('.tools/gitleaks-8.30.1/gitleaks'), gitLfs: path.resolve('.tools/git-lfs-3.8.1/git-lfs') });
      await closeWindow(app);
      app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    }
    let page = await app.firstWindow();
    // Native fingerprint confirmation has a 90-second budget, followed by five seconds to connect.
    await expect(page.locator('.status-bar')).toContainText('Daemon connected', { timeout: 95_000 });
    daemonPid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => {
      const picks = paths.map(file => ({ canceled: false, filePaths: [file] }));
      dialog.showOpenDialog = async () => picks.shift()!;
    }, [model, python]);
    const editorStartedAt = Date.now();
    await configureEditor(app, data, model, python);
    // Include the four ten-second startup attempts, shutdown waits and 1/2/4-second backoff.
    await expect.poll(async () => {
      const editor = (await page.evaluate(() => window.intercat.getStatus())).editor;
      editorStartup.push({ elapsedMs: Date.now() - editorStartedAt, editor });
      return editor?.phase;
    }, { timeout: 60_000 }).toBe('ready');
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready');
    await writeFile(test.info().outputPath('editor-startup.json'), JSON.stringify(editorStartup, null, 2));
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Wait for my signal.');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect(page.getByRole('row')).toHaveCount(1);
    await page.getByRole('textbox', { name: 'Request draft' }).fill('An ambiguous instruction');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await page.getByRole('button', { name: 'Hold', exact: true }).click();
    await expect(page.locator('.request-chip.held')).toHaveCount(1);
    await page.getByRole('button', { name: 'Undo', exact: true }).click();
    await expect(page.getByRole('group', { name: 'Request needs attention', exact: true })).toBeVisible();
    await page.getByRole('button', { name: 'Hold', exact: true }).click();
    await expect(page.locator('.request-chip.held')).toHaveCount(1);
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Retained native draft');
    const draftSession = (await page.evaluate(() => window.intercat.getStatus())).session!.id;
    await expect.poll(async () => (await page.evaluate(session => window.intercat.readDrafts(session), draftSession)).book.entries.work.text).toBe('Retained native draft');
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.locator('.request-chip.held')).toHaveCount(1);
    expect((await page.evaluate(() => window.intercat.getStatus())).daemonPid).toBe(daemonPid);
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Retained native draft');
    await page.getByRole('button', { name: 'Start request: An ambiguous instruction', exact: true }).click();
    await page.getByRole('button', { name: 'Worker A', exact: true }).click();
    await expect(page.locator('.request-chip.placed')).toHaveCount(2);
    const state = await page.evaluate(() => window.intercat.getStatus());
    const word = state.utterances!.find(word => word.text === 'An ambiguous instruction')!;
    expect(word.target).toEqual({ mode: 'lock', trackId: state.tracks![0].id });
    expect(word.decision?.kind).toBe('extend');
    expect(word.revision).toBe(6);
    expect(state.tracks).toHaveLength(1);

    await page.locator('.request-select').filter({ hasText: 'An ambiguous instruction' }).click();
    await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Native held edit');
    await expect.poll(async () => (await page.evaluate(session => window.intercat.readDrafts(session), draftSession)).book.entries[`edit:${word.id}`]?.text).toBe('Native held edit');
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.getByLabel('Request being edited')).toBeVisible();
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Native held edit');
    await page.getByRole('button', { name: 'Update request', exact: false }).click();
    await expect(page.locator('.request-select').filter({ hasText: 'Native held edit' })).toBeVisible();
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Retained native draft');

    await page.locator('.request-select').filter({ hasText: 'Native held edit' }).click();
    await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Native retry after restart');
    await expect.poll(async () => (await page.evaluate(session => window.intercat.readDrafts(session), draftSession)).book.entries[`edit:${word.id}`]?.text).toBe('Native retry after restart');
    const finish = await page.evaluate(async session => {
      const saved = await window.intercat.readDrafts(session);
      const entry = saved.book.entries[saved.book.active];
      if (entry.context.kind !== 'edit' || !entry.context.lease) throw new Error('Expected an active edit lease');
      const { token, owner, revision } = entry.context.lease;
      const request = { kind: 'end' as const, token, revision, edit: { kind: 'text' as const, text: entry.text } };
      entry.pending = { kind: 'edit', request };
      await window.intercat.writeDrafts({ session, revision: saved.revision, parentOpId: saved.opId, opId: await window.intercat.createOperationId(), book: saved.book });
      return { owner, action: request };
    }, draftSession);
    // Commit the saved request through real RPC while withholding its acknowledgement from the UI.
    const completedRevision = await app.evaluate(async (_electron, input) => {
      const load = process.getBuiltinModule('module').createRequire(input.rpc);
      const { RpcClient } = load(input.rpc);
      const { AsyncEntry } = load('@napi-rs/keyring');
      const token = await new AsyncEntry('Intercat', 'rpc.ui').getPassword();
      const client = await RpcClient.connect(input.socket, token);
      try { return (await client.request('utterance.edit', input.finish)).revision; }
      finally { client.close(); }
    }, { rpc: path.resolve('electron/rpc-client.mjs'), socket: path.join(data, 'daemon/run/rpc.sock'), finish });
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await page.getByRole('button', { name: 'Retry save', exact: false }).click();
    await expect(page.getByLabel('Request being edited')).toHaveCount(0);
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Retained native draft');
    const completed = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(item => item.id === word.id)!;
    expect(completed.text).toBe('Native retry after restart');
    expect(completed.revision).toBe(completedRevision);

    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(item => item.id === word.id)?.reading?.state).toBe('complete');
    const beforeCut = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(item => item.id === word.id)!;
    if (beforeCut.state === 'asked') {
      await page.locator(`[data-attention-card="request-${word.id}"]`).getByRole('button', { name: 'Take back', exact: true }).click();
    } else {
      await page.getByRole('button', { name: 'Cut request: Native retry after restart', exact: true }).click();
    }
    await expect(page.getByText('Taken back · 1', { exact: true })).toBeVisible();
    await page.getByRole('heading', { name: 'Wait for my signal.', exact: true }).click();
    await page.keyboard.press('Meta+z');
    // An edited request can require confirmation again after Undo restores it.
    await expect(page.locator('.request-select').filter({ hasText: 'Native retry after restart' }).or(
      page.getByRole('group', { name: 'Request needs attention', exact: true }).filter({ hasText: 'Native retry after restart' }),
    )).toBeVisible();
    const restored = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(item => item.id === word.id)!;
    expect(['asked', 'placed']).toContain(restored.state);
    expect(restored.target).toEqual({ mode: 'lock', trackId: state.tracks![0].id });
    await page.keyboard.press('Meta+Shift+z');
    await page.getByText('Taken back · 1', { exact: true }).click();
    await page.getByRole('button', { name: 'Restore to draft', exact: true }).click();
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Native retry after restart');
    await page.getByRole('button', { name: 'Cancel edit', exact: true }).click();
    await expect(page.getByRole('textbox', { name: 'Request draft' })).toHaveValue('Retained native draft');
    await page.getByRole('button', { name: 'Undo', exact: true }).click();
    await page.getByText('Taken back · 1', { exact: true }).click();
    await page.getByRole('button', { name: 'Restore held', exact: true }).click();
    await expect(page.locator('.request-select').filter({ hasText: 'Native retry after restart' })).toContainText('held');
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await page.getByRole('button', { name: 'Undo', exact: true }).click();
    await expect(page.getByText('Taken back · 1', { exact: true })).toBeVisible();
    const originalPaste = 'Native pasted request\n' + 'Preserve every source line. '.repeat(28) + '\nExplain only. Do not change files.';
    const nativeDraft = page.getByRole('textbox', { name: 'Request draft', exact: true });
    await nativeDraft.fill('');
    await nativeDraft.evaluate((element, text) => {
      const clipboardData = new DataTransfer(); clipboardData.setData('text/plain', text);
      element.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true, clipboardData }));
    }, originalPaste);
    const pasteStartedAt = Date.now();
    // The local scanner has a 35-second execution limit before its storage acknowledgement.
    await expect(page.getByText('Read original pasted text', { exact: true })).toBeVisible({ timeout: 45_000 });
    await writeFile(test.info().outputPath('paste-import.json'), JSON.stringify({ elapsedMs: Date.now() - pasteStartedAt }, null, 2));
    await page.getByText('Read original pasted text', { exact: true }).click();
    await expect(page.locator('.pasted-source pre')).toHaveText(originalPaste);
    await expect.poll(async () => (await page.evaluate(session => window.intercat.readDrafts(session), draftSession)).book.entries.work.sources?.[0]?.attachment?.id).toBeTruthy();
    await page.getByRole('button', { name: 'Recheck pasted text 1', exact: true }).click();
    await expect(page.getByText('Saved the screening check.', { exact: true })).toBeVisible({ timeout: 45_000 });
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.getByRole('button', { name: 'Keep as the request', exact: true })).toHaveAttribute('aria-pressed', 'true');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    const pastedWord = page.locator('.request-select').filter({ hasText: 'Native pasted request' });
    await expect(pastedWord).toBeVisible();
    await expect.poll(async () => (await page.evaluate(() => window.intercat.getStatus())).utterances?.find(item => item.sources?.length)?.decision?.kind).toBe('fork');
    const pastedStatus = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(item => item.sources?.length)!;
    expect(pastedStatus.sourceText).toBe('');
    expect(pastedStatus.attachments).toHaveLength(1);
    expect(pastedStatus.decision?.kind).toBe('fork');
    await pastedWord.click();
    await page.getByRole('dialog', { name: 'Request', exact: true }).getByRole('button', { name: 'Edit text', exact: true }).click();
    await page.getByRole('button', { name: 'Attach as reference', exact: true }).click();
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Explain native reference');
    await page.getByRole('button', { name: 'Update request', exact: false }).click();
    await expect(page.getByLabel('Request being edited')).toHaveCount(0);
    const editedPaste = (await page.evaluate(() => window.intercat.getStatus())).utterances!.find(item => item.id === pastedStatus.id)!;
    expect(editedPaste.sources?.[0]?.role).toBe('reference');
    expect(editedPaste.attachments[0].id).toBe(pastedStatus.attachments[0].id);

    await mkdir('research/verify/request-flow-20261104', { recursive: true });
    await page.screenshot({ path: 'research/verify/request-flow-20261104/resolved.png' });
  } catch (error) {
    const state = await Promise.race([
      (await app.firstWindow()).evaluate(() => window.intercat.getStatus()).catch(() => null),
      new Promise<null>(resolve => setTimeout(() => resolve(null), 3000)),
    ]);
    const log = await readFile(path.join(data, 'daemon/editor.log'), 'utf8').catch(() => '');
    const daemonLog = await readFile(path.join(data, 'daemon/daemon.log'), 'utf8').catch(() => '');
    const page = await app.firstWindow();
    const composer = await page.locator('.composer').innerText().catch(() => 'unavailable');
    const notice = await page.locator('.toast').innerText().catch(() => 'unavailable');
    await writeFile(test.info().outputPath('native-editor.json'), JSON.stringify({ editor: state?.editor, editorStartup, connected: state?.connected, message: state?.message, composer, notice, log, daemonLog }, null, 2));
    throw error;
  } finally {
    await closeWindow(app);
    if (daemonPid) process.kill(daemonPid, 'SIGTERM');
    await rm(data, { recursive: true, force: true });
  }
});
