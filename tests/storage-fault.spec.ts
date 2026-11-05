import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';

const execute=promisify(execFile);
const alive=(pid:number)=>{try{process.kill(pid,0);return true;}catch{return false;}};
for (const mode of ['full', 'slow-write', 'crash-full', 'boundary-crash'] as const) test(`Storage fault ${mode}: Stop precedes writes and no held input runs after recovery`,async()=>{
  test.skip(process.env.INTERCAT_M1_FAULT!=='1','Opt-in fault-injection build and one paid disposable turn');
  test.setTimeout(240000);
  const data=await mkdtemp('/private/tmp/ic-full-');
  const output=path.resolve(process.env.INTERCAT_M1_OUTPUT!,mode);
  const source=path.join(data,'source');await mkdir(source);await mkdir(output,{recursive:true});
  const env={PATH:process.env.PATH,HOME:data,GIT_CONFIG_GLOBAL:'/dev/null',GIT_CONFIG_NOSYSTEM:'1'};
  await execute('/usr/bin/git',['init','-b','main',source],{env});
  await writeFile(path.join(source,'wait.py'),"import os,pathlib,time\npathlib.Path('waiting.pid').write_text(str(os.getpid()))\ndeadline=time.time()+120\nwhile not pathlib.Path('release').exists() and time.time()<deadline: time.sleep(.05)\npathlib.Path('finished').write_text('done')\n");
  await execute('/usr/bin/git',['-C',source,'add','.'],{env});
  await execute('/usr/bin/git',['-C',source,'-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-m','Storage failure fixture'],{env});
  let app=await electron.launch({args:['.','--user-data-dir='+data]});
  let daemon:number|undefined,full=false;
  const observations:Record<string,unknown>={data,mode,passed:false};
  const fault=async(action:'fill'|'restore'|'pause')=>app.evaluate(async(_electron,input)=>{
    const load=process.getBuiltinModule('module').createRequire(input.module);
    const {RpcClient}=load(input.rpc);const {uiToken}=load(input.daemon);
    const client=await RpcClient.connect(input.socket,await uiToken());
    try{return await client.request('test.writer',{action:input.action,...(input.action==='pause'?{millis:2000}:{})});}finally{client.close();}
  },{module:path.resolve('electron/main.mjs'),rpc:path.resolve('electron/rpc-client.mjs'),daemon:path.resolve('electron/daemon.mjs'),socket:path.join(data,'daemon/run/rpc.sock'),action});
  try{
    const page=await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
    daemon=(await page.evaluate(()=>window.intercat.getStatus())).daemonPid;
    expect(await fault('restore')).toEqual({restored:true});
    await app.evaluate(({dialog},paths)=>{const picks=paths.map(file=>({canceled:false,filePaths:[file]}));dialog.showOpenDialog=async()=>picks.shift()!;},[source,process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button',{name:'Choose project folder',exact:true}).click();await configureEditor(app,data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready',{timeout:60000});
    await page.getByRole('button',{name:'Connect workers',exact:true}).click();
    const draft=page.getByRole('textbox',{name:'Request draft'});
    await draft.fill('Start now. Use bash to execute exactly /usr/bin/python3 wait.py once and wait for it to finish. Do not run other commands or edit files.');
    await page.getByRole('button',{name:'New track',exact:true}).click();
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).workspaces?.length,{timeout:30000}).toBe(1);
    const workspace=(await page.evaluate(()=>window.intercat.getStatus())).workspaces![0].path;
    await expect.poll(async()=>{try{return Number(await readFile(path.join(workspace,'waiting.pid'),'utf8'))>0;}catch{return false;}},{timeout:120000}).toBe(true);
    const child=Number(await readFile(path.join(workspace,'waiting.pid'),'utf8'));
    await draft.fill('After this turn, add one line to CHANGELOG.');await page.getByRole('button',{name:'Put',exact:true}).click();
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('placed');
    observations.before=await page.evaluate(()=>window.intercat.getStatus());
    if(mode==='slow-write') {
      const pausedAt=Date.now();
      observations.fault=await fault('pause');
      await draft.fill('This write was queued before Stop all.');
      await page.getByRole('button',{name:'Put',exact:true}).click();
      await expect(page.getByRole('button',{name:'Saving…',exact:false})).toBeVisible();
      const started=Date.now();await page.keyboard.press('Alt+Meta+.');
      await expect.poll(()=>alive(child),{timeout:1800}).toBe(false);
      observations.stopMs=Date.now()-started;
      observations.stopBeforeWriterResumed=Date.now()-pausedAt<2000;
      expect(observations.stopBeforeWriterResumed).toBe(true);
      await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).dispatchPaused).toBe(true);
      const restored=await page.evaluate(()=>window.intercat.getStatus());
      expect(restored.utterances?.find(w=>w.text==='This write was queued before Stop all.')?.state).toBe('held');
      expect(restored.utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
      expect(restored.unloggedControls?.length).toBeGreaterThan(0);
      observations.restored=restored;observations.passed=true;
      await page.screenshot({path:path.join(output,'recovered.png')});
      return;
    }
    observations.fault=await fault('fill');full=true;
    expect(observations.fault).toMatchObject({errorCode:'SQLITE_FULL',databasePagesUnchanged:true});
    await draft.fill('This draft must remain not saved.');await page.getByRole('button',{name:'Put',exact:true}).click();
    await expect(page.getByRole('button',{name:'Retry save',exact:true})).toBeVisible();
    await expect(draft).toHaveValue('This draft must remain not saved.');
    const started=Date.now();
    if(mode==='boundary-crash') {
      await page.keyboard.press('Shift+Meta+.');
      expect(alive(child)).toBe(true);
      await writeFile(path.join(workspace,'release'),'finish normally');
      await expect.poll(async()=>{try{return await readFile(path.join(workspace,'finished'),'utf8');}catch{return '';}}).toBe('done');
      observations.shellFinishedNormally=true;
      const worker=(observations.before as import('../src/bridge').DaemonStatus).workers![0];
      await expect.poll(async()=>{
        const result=await execute('/usr/bin/python3',['-c',`import json,pathlib,sys
root=pathlib.Path(sys.argv[1])/'.local/share/muse/sessions'
terminal=''
for file in root.glob('*/*/*/'+sys.argv[2]+'/session.jsonl'):
 for line in file.read_text().splitlines():
  try: row=json.loads(line)
  except ValueError: continue
  value=row.get('payload',{})
  if value.get('kind')=='run' and value.get('run_id')==sys.argv[3] and value.get('event',{}).get('kind')=='terminal': terminal=value['event'].get('terminal','')
print(terminal)`,process.env.HOME!,worker.sessionId!,worker.activeTurn!]);
        return result.stdout.trim();
      },{timeout:30000}).toBe('completed');
      observations.nativeTerminal='completed';
    } else {
      await page.getByRole('button',{name:'Stop Worker A',exact:true}).click();
      await expect.poll(()=>alive(child),{timeout:5000}).toBe(false);
      observations.stopMs=Date.now()-started;
      expect(observations.stopMs).toBeLessThan(5000);
    }
    observations.during=await page.evaluate(()=>window.intercat.getStatus());
    await page.screenshot({path:path.join(output,'full-stopped.png')});
    if(mode==='crash-full'||mode==='boundary-crash') {
      await closeWindow(app);
      process.kill(daemon!,'SIGKILL');daemon=undefined;full=false;
      const oldEditor=(observations.before as import('../src/bridge').DaemonStatus).editor?.pid;
      if(oldEditor) await expect.poll(()=>alive(oldEditor),{timeout:5000}).toBe(false);
      observations.editorExitedWithDaemon=true;
      app=await electron.launch({args:['.','--user-data-dir='+data]});
      const reopened=await app.firstWindow();
      await expect(reopened.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
      daemon=(await reopened.evaluate(()=>window.intercat.getStatus())).daemonPid;
      await expect.poll(async()=>(await reopened.evaluate(()=>window.intercat.getStatus())).workers?.[0]?.phase,{timeout:15000}).toBe('needs-recovery');
      const restored=await reopened.evaluate(()=>window.intercat.getStatus());
      expect(restored.utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
      expect(restored.utterances?.some(w=>w.text==='This draft must remain not saved.')).toBe(false);
      expect(restored.workers?.[0]?.sessionId).toBe((observations.before as typeof restored).workers?.[0]?.sessionId);
      await reopened.waitForTimeout(2500);
      expect((await reopened.evaluate(()=>window.intercat.getStatus())).workers?.[0]?.phase).toBe('needs-recovery');
      observations.restored=restored;observations.passed=true;
      await reopened.screenshot({path:path.join(output,'after-restart.png')});
      return;
    }
    await fault('restore');full=false;
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).utterances?.find(w=>w.text.includes('CHANGELOG'))?.state,{timeout:15000}).toBe('held');
    const restored=await page.evaluate(()=>window.intercat.getStatus());
    expect(restored.utterances?.some(w=>w.text==='This draft must remain not saved.')).toBe(false);
    expect(restored.unloggedControls?.length).toBeGreaterThan(0);
    await expect(page.locator('.unlogged-control')).toContainText('unlogged');
    await page.screenshot({path:path.join(output,'recovered.png')});
    observations.restored=restored;observations.passed=true;
  }finally{
    if(full){try{await fault('restore');}catch{}}
    await writeFile(path.join(output,'observations.json'),JSON.stringify(observations,null,2));
    try{await execute('/usr/bin/python3',['-c','import sqlite3,sys; sqlite3.connect(sys.argv[1]).backup(sqlite3.connect(sys.argv[2]))',path.join(data,'daemon/events.sqlite'),path.join(output,'events.sqlite')]);}catch{}
    await closeWindow(app);if(daemon&&alive(daemon))process.kill(daemon,'SIGTERM');
  }
});
