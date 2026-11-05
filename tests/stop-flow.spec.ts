import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';

const execute = promisify(execFile);
const alive = (pid:number) => { try { process.kill(pid,0); return true; } catch { return false; } };

test('Stop all holds follow-ups, preserves a detached server, and Quit reclaims it after host loss', async () => {
  test.skip(process.env.INTERCAT_M1_STOP !== '1', 'Opt-in two paid dummy worker turns');
  test.setTimeout(360000);
  const data = await mkdtemp('/private/tmp/ic-stop-acceptance-');
  const output = path.resolve(process.env.INTERCAT_M1_OUTPUT!);
  const source = path.join(data,'source');
  await mkdir(source); await mkdir(output,{recursive:true});
  const env = {PATH:process.env.PATH,HOME:data,GIT_CONFIG_GLOBAL:'/dev/null',GIT_CONFIG_NOSYSTEM:'1'};
  await execute('/usr/bin/git',['init','-b','main',source],{env});
  await writeFile(path.join(source,'server.py'),`import http.server,json,os,pathlib,subprocess,sys,time
if len(sys.argv)>1:
 server=http.server.HTTPServer(('127.0.0.1',0),http.server.SimpleHTTPRequestHandler)
 pathlib.Path('server-state.json').write_text(json.dumps({'pid':os.getpid(),'port':server.server_port}))
 server.serve_forever()
else:
 with open('server.log','w') as output:
  child=subprocess.Popen([sys.executable,__file__,'serve'],stdin=subprocess.DEVNULL,stdout=output,stderr=output,start_new_session=True)
 while not pathlib.Path('server-state.json').exists():
  if child.poll() is not None: raise RuntimeError('Server failed; see server.log')
  time.sleep(.05)
 print('Server ready',flush=True)
 time.sleep(12)
`);
  await writeFile(path.join(source,'wait.py'),"import os,pathlib,time\npathlib.Path('waiting.pid').write_text(str(os.getpid()))\ntime.sleep(120)\n");
  await execute('/usr/bin/git',['-C',source,'add','.'],{env});
  await execute('/usr/bin/git',['-C',source,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-m','Detached server fixture'],{env});
  const app = await electron.launch({args:['.','--user-data-dir='+data]});
  let daemon:number|undefined, serverPid:number|undefined;
  const observations:Record<string,unknown>={data,passed:false};
  try {
    const page=await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
    daemon=(await page.evaluate(()=>window.intercat.getStatus())).daemonPid;
    await app.evaluate(({dialog},paths)=>{
      const picks=paths.map(file=>({canceled:false,filePaths:[file]}));
      dialog.showOpenDialog=async()=>picks.shift()!;
    },[source,process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button',{name:'Choose project folder',exact:true}).click();
    await configureEditor(app,data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready',{timeout:60000});
    await page.getByRole('button',{name:'Connect workers',exact:true}).click();
    await page.getByRole('textbox',{name:'Request draft'}).fill('Start now. Use bash with sandbox_permissions="require_escalated" to execute exactly /usr/bin/python3 server.py once, and wait for it to finish. The supplied local-only server fixture deliberately waits. Do not edit files, retry, or run other commands.');
    await page.getByRole('button',{name:'New track',exact:true}).click();
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).workspaces?.length,{timeout:30000}).toBe(1);
    const workspace=(await page.evaluate(()=>window.intercat.getStatus())).workspaces![0].path;
    const stateFile=path.join(workspace,'server-state.json');
    const approval=page.getByRole('group',{name:'Approval for Worker A'});
    await expect(approval).toContainText('server.py',{timeout:120000});
    await approval.getByRole('button',{name:'Allow once',exact:true}).click();
    await expect.poll(async()=>{try{return JSON.parse(await readFile(stateFile,'utf8')).pid>0;}catch{return false;}},{timeout:120000}).toBe(true);
    const server=JSON.parse(await readFile(stateFile,'utf8')) as {pid:number;port:number};
    serverPid=server.pid;
    expect((await fetch('http://127.0.0.1:'+server.port)).ok).toBe(true);
    await page.getByRole('textbox',{name:'Request draft'}).fill('After this turn, add one line to CHANGELOG.');
    await page.getByRole('button',{name:'Put',exact:true}).click();
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('placed');
    await page.getByRole('textbox',{name:'Request draft'}).press('Shift+Meta+.');
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).workers?.[0]?.activeTurn,{timeout:45000}).toBeNull();
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).workers?.[0]?.checkpointPending).toBe(false);
    const boundary=await page.evaluate(()=>window.intercat.getStatus());
    observations.boundary=boundary;
    expect(boundary.utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
    expect(boundary.tracks?.[0].status).toBe('done');
    expect((await fetch('http://127.0.0.1:'+server.port)).ok).toBe(true);
    await page.getByRole('textbox',{name:'Request draft'}).fill('Start now. Use bash to execute exactly /usr/bin/python3 wait.py once and wait for it to finish. Do not edit files, retry, or run other commands.');
    await page.getByRole('button',{name:'Put',exact:true}).click();
    await expect.poll(async()=>{try{return Number(await readFile(path.join(workspace,'waiting.pid'),'utf8'))>0;}catch{return false;}},{timeout:120000}).toBe(true);
    const renewed=await page.evaluate(()=>window.intercat.getStatus());
    observations.renewed=renewed;
    expect(renewed.workers![0].sessionId).toBe(boundary.workers![0].sessionId);
    expect(renewed.utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
    await page.getByRole('textbox',{name:'Request draft'}).fill('After this turn, record a second note.');
    await page.getByRole('button',{name:'Put',exact:true}).click();
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).utterances?.find(w=>w.text.includes('second note'))?.state).toBe('placed');
    await page.getByRole('textbox',{name:'Request draft'}).press('Alt+Meta+.');
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).dispatchPaused).toBe(true);
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).workers?.[0]?.activeTurn,{timeout:30000}).toBeNull();
    await expect(page.getByRole('button',{name:'Resume',exact:true})).toBeEnabled({timeout:30000});
    expect((await fetch('http://127.0.0.1:'+server.port)).ok).toBe(true);
    const paused=await page.evaluate(()=>window.intercat.getStatus());
    observations.paused=paused;
    expect(paused.tracks?.[0].status).toBe('stopped');
    await expect(page.locator('.track-status.stopped')).toContainText('■ stopped');
    expect(paused.utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
    expect(paused.utterances?.find(w=>w.text.includes('second note'))?.state).toBe('held');
    expect(paused.runningProcesses).toBeGreaterThan(0);
    await page.getByRole('button',{name:'Resume',exact:true}).click();
    await expect.poll(async()=> (await page.evaluate(()=>window.intercat.getStatus())).dispatchPaused).toBe(false);
    expect((await page.evaluate(()=>window.intercat.getStatus())).utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
    // Re-pause while the selected worker is already done; the status control is retained when paused.
    await page.getByRole('textbox',{name:'Request draft'}).press('Alt+Meta+.');
    await expect(page.getByRole('button',{name:/Stop all turns/})).toBeVisible();
    const host=(await page.evaluate(()=>window.intercat.getStatus())).workers![0].identity!;
    expect(alive(host.pid)).toBe(true);
    process.kill(host.pid,'SIGKILL');
    await expect.poll(()=>alive(host.pid)).toBe(false);
    expect((await fetch('http://127.0.0.1:'+server.port)).ok).toBe(true);
    observations.hostKilled=host; observations.server=server;
    await page.screenshot({path:path.join(output,'paused-after-host-loss.png')});
    const dialogs:unknown[]=[];
    app.on('console',message=>{if(message.text().startsWith('STOP_FIXTURE:'))dialogs.push(JSON.parse(message.text().slice(13)));});
    await app.evaluate(({dialog})=>{
      dialog.showMessageBox=async(value:import('electron').BaseWindow|import('electron').MessageBoxOptions,options?:import('electron').MessageBoxOptions)=>{
        const shown='message' in value ? value : options!;
        console.log('STOP_FIXTURE:'+JSON.stringify(shown));
        return {response:shown.message==='Quit Intercat?'?1:2,checkboxChecked:false};
      };
    });
    const closed=app.waitForEvent('close');
    await app.evaluate(({app})=>app.quit());
    await closed;
    expect(alive(daemon!)).toBe(false); daemon=undefined;
    await expect.poll(()=>alive(server.pid),{timeout:5000}).toBe(false); serverPid=undefined;
    await expect(fetch('http://127.0.0.1:'+server.port)).rejects.toThrow();
    observations.dialogs=dialogs; observations.passed=true;
  } finally {
    await writeFile(path.join(output,'observations.json'),JSON.stringify(observations,null,2));
    try {await execute('/usr/bin/python3',['-c','import sqlite3,sys; sqlite3.connect(sys.argv[1]).backup(sqlite3.connect(sys.argv[2]))',path.join(data,'daemon/events.sqlite'),path.join(output,'events.sqlite')]);}catch{}
    await closeWindow(app);
    if(daemon && alive(daemon))process.kill(daemon,'SIGTERM');
    if(serverPid && alive(serverPid))process.kill(serverPid,'SIGKILL');
  }
});
