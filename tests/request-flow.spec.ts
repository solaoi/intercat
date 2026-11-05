import { closeWindow } from './support/close';
import { configureEditor } from './support/editor';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import path from 'node:path';

test('a held request survives closing the UI and a chosen destination is re-read', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in native daemon integration');
  const data = await mkdtemp('/private/tmp/ic-request-ui-');
  const moduleRoot = path.join(data, 'fixture');
  const packageRoot = path.join(moduleRoot, 'intercat_editor');
  const model = path.join(data, 'model');
  await mkdir(packageRoot, { recursive: true });
  await mkdir(model);
  await writeFile(path.join(packageRoot, '__init__.py'), '');
  // A deterministic local HTTP fixture tests the UI/RPC state machine, not model quality.
  await writeFile(path.join(packageRoot, 'server.py'), `import json
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def respond(self,value):
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self): self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  request=json.loads(self.rfile.read(int(self.headers['Content-Length'])));context=json.loads(request['tail']);selected={};stages={}
  for stage in request['stages']:
   if any(selected.get(key) not in values for key,values in stage.get('when',{}).items()): continue
   labels=stage['candidates'];choice=(' A' if context['target']['mode']=='lock' else ' F') if stage['name']=='decision' else ' D'
   selected[stage['name']]=choice;n=len(labels);stages[stage['name']]={'selected':choice,'logprobs':{x:-.1 if x==choice else -2 for x in labels},'probabilities':{x:1 if n==1 else .9 if x==choice else .1/(n-1) for x in labels}}
  self.respond({'stages':stages,'inferenceMs':10})
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
`);
  // The launcher always supplies the actual source path. Use a fixture Python wrapper to select our module.
  const python = path.join(data, 'python');
  await writeFile(python, `#!/bin/sh\nPYTHONPATH='${moduleRoot}' exec /usr/bin/python3 "$@"\n`, { mode: 0o700 });
  let app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
  let daemonPid: number | undefined;
  try {
    let page = await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected');
    daemonPid = (await page.evaluate(() => window.intercat.getStatus())).daemonPid;
    await app.evaluate(({ dialog }, paths) => {
      const picks = paths.map(file => ({ canceled: false, filePaths: [file] }));
      dialog.showOpenDialog = async () => picks.shift()!;
    }, [model, python]);
    await configureEditor(app, data, model, python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready');
    await page.getByRole('textbox', { name: 'Request draft' }).fill('Wait for my signal.');
    await page.getByRole('button', { name: 'New track', exact: true }).click();
    await expect(page.getByRole('row')).toHaveCount(1);
    await page.getByRole('textbox', { name: 'Request draft' }).fill('An ambiguous instruction');
    await page.getByRole('button', { name: 'Put', exact: true }).click();
    await page.getByRole('button', { name: 'Hold', exact: true }).click();
    await expect(page.locator('.request-chip.held')).toHaveCount(1);
    await closeWindow(app);
    app = await electron.launch({ args: ['.', `--user-data-dir=${data}`] });
    page = await app.firstWindow();
    await expect(page.locator('.request-chip.held')).toHaveCount(1);
    expect((await page.evaluate(() => window.intercat.getStatus())).daemonPid).toBe(daemonPid);
    await page.getByRole('button', { name: 'Start request: An ambiguous instruction', exact: true }).click();
    await page.getByRole('button', { name: 'Worker A', exact: true }).click();
    await expect(page.locator('.request-chip.placed')).toHaveCount(2);
    const state = await page.evaluate(() => window.intercat.getStatus());
    const word = state.utterances!.find(word => word.text === 'An ambiguous instruction')!;
    expect(word.target).toEqual({ mode: 'lock', trackId: state.tracks![0].id });
    expect(word.decision?.kind).toBe('extend');
    expect(word.revision).toBe(4);
    expect(state.tracks).toHaveLength(1);
    await mkdir('research/verify/request-flow-20261104', { recursive: true });
    await page.screenshot({ path: 'research/verify/request-flow-20261104/resolved.png' });
  } finally {
    await closeWindow(app);
    if (daemonPid) process.kill(daemonPid, 'SIGTERM');
    await rm(data, { recursive: true, force: true });
  }
});
