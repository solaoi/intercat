import { _electron as electron, expect, test } from '@playwright/test';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdir, mkdtemp, readFile, readdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';

const execute=promisify(execFile);
const alive=(pid:number)=>{try{process.kill(pid,0);return true;}catch{return false;}};

test('a slow Check does not block another approval or Stop and cannot turn green after Quit',async()=>{
  test.skip(process.env.INTERCAT_M1_CONCURRENT!=='1','Opt-in three paid disposable worker turns');
  test.setTimeout(360000);
  const data=await mkdtemp('/private/tmp/ic-concurrent-');
  const output=path.resolve(process.env.INTERCAT_M1_OUTPUT!);
  const source=path.join(data,'source'), remote=path.join(data,'remote.git');
  await mkdir(source);await mkdir(output,{recursive:true});
  const env={PATH:process.env.PATH,HOME:data,GIT_CONFIG_GLOBAL:'/dev/null',GIT_CONFIG_NOSYSTEM:'1'};
  const git=async(...args:string[])=>(await execute('/usr/bin/git',['-C',source,...args],{env})).stdout.trim();
  await git('init','-b','main');
  await writeFile(path.join(source,'value.txt'),'before\n');
  await writeFile(path.join(source,'package.json'),JSON.stringify({name:'concurrency-fixture',version:'1.0.0',private:true,scripts:{test:'node check.cjs'}}));
  await writeFile(path.join(source,'package-lock.json'),JSON.stringify({name:'concurrency-fixture',version:'1.0.0',lockfileVersion:3,requires:true,packages:{'':{name:'concurrency-fixture',version:'1.0.0'}}}));
  await writeFile(path.join(source,'check.cjs'),"const fs=require('node:fs');const assert=require('node:assert/strict');assert.equal(fs.readFileSync('value.txt','utf8'),'after\\n');fs.writeFileSync('check.running',String(process.pid));setTimeout(()=>{fs.unlinkSync('check.running');console.log('check passed');},30000);\n");
  await writeFile(path.join(source,'wait.py'),"import os,pathlib,time\npathlib.Path('waiting.pid').write_text(str(os.getpid()))\ntime.sleep(120)\n");
  await git('add','.');await git('-c','user.name=Fixture','-c','user.email=fixture@example.invalid','commit','-m','Concurrency acceptance fixture');
  await execute('/usr/bin/git',['init','--bare',remote],{env});await git('remote','add','origin',remote);
  let app=await electron.launch({args:['.','--user-data-dir='+data]});
  let daemon:number|undefined;
  const observations:Record<string,unknown>={data,passed:false};
  try{
    let page=await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
    daemon=(await page.evaluate(()=>window.intercat.getStatus())).daemonPid;
    await app.evaluate(({dialog},paths)=>{const picks=paths.map(file=>({canceled:false,filePaths:[file]}));dialog.showOpenDialog=async()=>picks.shift()!;},[source,process.env.INTERCAT_TEST_MUSE_BINARY!]);
    await page.getByRole('button',{name:'Choose project folder',exact:true}).click();await configureEditor(app,data);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready',{timeout:60000});
    await page.getByRole('button',{name:'Connect workers',exact:true}).click();
    const request=async(text:string,fresh=true)=>{await page.getByRole('textbox',{name:'Request draft'}).fill(text);await page.getByRole('button',{name:fresh?'New track':'Put',exact:true}).click();};
    await request('Start now. Use write_file to change only value.txt to exactly after followed by one newline. Do not run tests or commands, and do not edit other files. Report what changed.');
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).workers?.[0]?.checkpoint?.sha,{timeout:120000}).toBeTruthy();
    await request('Start now. Use bash to execute exactly git push origin HEAD:refs/heads/concurrent-test once. Do not run other commands or edit files. Report the result.');
    const approval=page.getByRole('group',{name:'Approval for Worker B'});
    await expect(approval).toContainText('push',{timeout:120000});
    await request('Start now. Use bash to execute exactly /usr/bin/python3 wait.py once and wait for it to finish. Do not edit files or run other commands.');
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).workspaces?.length).toBe(3);
    const waiting=(await page.evaluate(()=>window.intercat.getStatus())).workspaces![2].path;
    await expect.poll(async()=>{try{return Number(await readFile(path.join(waiting,'waiting.pid'),'utf8'))>0;}catch{return false;}},{timeout:120000}).toBe(true);
    await request('After this turn, add a note to CHANGELOG.',false);
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('placed');
    await page.getByRole('button',{name:'Commit',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Commit review'});
    const marker=await (async()=>{
      let found='';
      await expect.poll(async()=>{
        const job=(await page.evaluate(()=>window.intercat.getStatus())).integrations![0];
        if(!job)return false;
        const parent=path.join(data,'daemon/integration',job.id);
        const roots=job.candidate ? [job.candidate.workspace] : (await readdir(parent,{withFileTypes:true})).filter(entry=>entry.isDirectory()).map(entry=>path.join(parent,entry.name));
        for(const root of roots){const file=path.join(root,'repo/check.running');try{if(Number(await readFile(file,'utf8'))>0){found=file;return true;}}catch{}}
        return false;
      },{timeout:30000}).toBe(true);
      return found;
    })();
    const checkPid=Number(await readFile(marker,'utf8'));
    const approvalStart=Date.now();
    await page.keyboard.press('Control+Tab');
    await approval.getByRole('button',{name:'Don’t run it',exact:true}).focus();
    await page.keyboard.press('Enter');
    await expect(approval).toHaveCount(0);
    observations.approvalMs=Date.now()-approvalStart;
    const stopStart=Date.now();
    await page.getByRole('grid',{name:'Tracks',exact:true}).focus();await page.keyboard.press('3');await page.keyboard.press('Meta+.');
    await expect.poll(async()=>{const state=await page.evaluate(()=>window.intercat.getStatus());return state.workers?.find(w=>w.trackId===state.tracks?.[2].id)?.activeTurn;},{timeout:15000}).toBeNull();
    observations.stopMs=Date.now()-stopStart;
    expect(observations.approvalMs).toBeLessThan(10000);expect(observations.stopMs).toBeLessThan(10000);
    expect(alive(checkPid)).toBe(true);
    await page.keyboard.press('Control+Shift+Tab');
    await expect(sheet).toBeVisible();
    observations.duringCheck=await page.evaluate(()=>window.intercat.getStatus());
    await page.screenshot({path:path.join(output,'check-and-stop.png')});
    await app.evaluate(({dialog})=>{dialog.showMessageBox=async(value:import('electron').BaseWindow|import('electron').MessageBoxOptions,options?:import('electron').MessageBoxOptions)=>({response:('message' in value?value:options!).message==='Quit Intercat?'?1:2,checkboxChecked:false});});
    const closed=app.waitForEvent('close');await app.evaluate(({app})=>app.quit());await closed;
    expect(alive(daemon!)).toBe(false);daemon=undefined;
    expect(alive(checkPid)).toBe(false);
    app=await electron.launch({args:['.','--user-data-dir='+data]});page=await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
    const restored=await page.evaluate(()=>window.intercat.getStatus());daemon=restored.daemonPid;
    expect(restored.dispatchPaused).toBe(true);
    expect(restored.integrations![0]?.status).toBe('failed');
    expect(restored.utterances?.find(w=>w.text.includes('CHANGELOG'))?.state).toBe('held');
    expect(restored.workers?.every(w=>!['starting','working','reconnecting','approval'].includes(w.phase))).toBe(true);
    observations.restored=restored;observations.passed=true;
  }finally{
    await writeFile(path.join(output,'observations.json'),JSON.stringify(observations,null,2));
    try{await execute('/usr/bin/python3',['-c','import sqlite3,sys; sqlite3.connect(sys.argv[1]).backup(sqlite3.connect(sys.argv[2]))',path.join(data,'daemon/events.sqlite'),path.join(output,'events.sqlite')]);}catch{}
    await closeWindow(app);if(daemon&&alive(daemon))process.kill(daemon,'SIGTERM');
  }
});
