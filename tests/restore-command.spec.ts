import {_electron as electron,expect,test,type ElectronApplication} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';

async function fixture(app:ElectronApplication,zoom:number,mode:'multiple'|'single'|'held'|'empty') {
  await mockDrafts(app);
  await app.evaluate(({ipcMain,BrowserWindow},{zoom,mode})=>{
    const ids=Array.from({length:10},(_,index)=>`01960000-0000-7000-8000-${String(index+1).padStart(12,'0')}`);
    const [a,b,first,second,operation,receipt,session,folder,request]=ids;
    const entries=[{id:first,track:a,label:'A'},{id:second,track:b,label:'B'}].map(entry=>({...entry,job:'own-publication',publication:'own-effect',session,folder,kind:'aside',phase:'aside',error:null}));
    const candidates=entries.map(entry=>({id:entry.id,track:entry.track,label:entry.label,title:`Own saved ${entry.label}`,session,folder}));
    const presentation={command:'restore',operationId:operation,receiptId:receipt,trackId:mode==='held'?a:null,restore:mode==='empty'?[]:mode==='multiple'?candidates:candidates.slice(0,1)};
    const command:any={id:request,text:'Restore the saved path',state:mode==='held'?'held':'placed',revision:1,createdAt:2,target:{mode:'none'},attachments:[],reason:null,decision:{kind:'command',command:'restore',trackId:mode==='held'?a:null,boundary:null},presentation:mode==='held'?null:presentation};
    const held={id:'own-held-work',text:'Own held work',state:'held',revision:4,createdAt:1,target:{mode:'lock',trackId:a},attachments:[],reason:null,decision:{kind:'schedule',trackId:a,boundary:'when-told'}};
    const state:any={connected:true,sequence:1,workerConfigured:true,session:{id:session,name:'Own command fixture'},tracks:[{id:a,label:'A',title:'Own saved A',status:'done'},{id:b,label:'B',title:'Own saved B',status:'done'}],workers:entries.map(entry=>({trackId:entry.track,sessionId:`own-${entry.label}`,activeTurn:null,phase:'held',checkpointPending:false,approvals:[],prompts:[]})),pathRetirements:entries,pathRestores:[],restoreFollowups:[],repairRuns:[],integrations:[],realignments:[],pendingRealignments:[],workspaces:[],utterances:[held,command],editor:{phase:'ready',accepting:true},privacy:{mode:'basic',revision:0}};
    const data:any={state,command,presentation,entries,receipts:[],requests:[],starts:[],failReceipt:mode==='single'||mode==='multiple'};Object.assign(globalThis,{restoreCommandFixture:data});
    const handle=(name:string,body:(_event:unknown,value:any)=>unknown)=>{ipcMain.removeHandler('intercat:'+name);ipcMain.handle('intercat:'+name,body);};
    handle('status',()=>state);
    handle('timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
    handle('utterance-read',(_event,id)=>{if(id!==command.id)throw new Error('Wrong own command');return command;});
    handle('resolve',(_event,value)=>{if(value.utteranceId!==command.id||value.opId!==receipt||value.resolution.action!=='presented')throw new Error('Wrong own display receipt');data.receipts.push(value);if(data.failReceipt){data.failReceipt=false;throw new Error('Own display receipt unavailable');}command.state='landed';command.revision++;command.presentation=null;state.sequence++;return command;});
    handle('start-held',(_event,value)=>{if(value.utteranceId!==command.id||command.state!=='held')throw new Error('Only the own held display can open');data.starts.push(value);command.state='placed';command.revision++;command.presentation=presentation;state.sequence++;return command;});
    handle('path-restore',(_event,value)=>{if(!presentation.restore.some(candidate=>candidate.id===value.retirementId))throw new Error('Wrong captured own path');data.requests.push(value);const entry=state.pathRetirements.find((entry:any)=>entry.id===value.retirementId);if(!entry)throw new Error('Own path no longer aside');const job={request:value,trackId:entry.track,sessionId:session,folderId:folder,revision:1,phase:'done',keepProcesses:false,error:null,inspection:null,mergeInput:null,restored:{trackId:entry.track,sessionId:`own-${entry.label}`,previousBase:'a'.repeat(40),base:'b'.repeat(40),checkpoint:null,suggestRealign:false}};state.pathRestores=[job];state.pathRetirements=state.pathRetirements.filter((item:any)=>item.id!==entry.id);state.sequence++;return job;});
    const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
  },{zoom,mode});
  const page=await app.firstWindow();await page.reload();return page;
}
async function capture(app:ElectronApplication,name:string) {
  const directory=path.resolve('research/verify/m4-restore-command-ui-20261110-01');await mkdir(directory,{recursive:true});
  const page=await app.firstWindow();await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
  const bytes=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
  await writeFile(path.join(directory,name+'.png'),Buffer.from(bytes,'base64'));
}

for(const zoom of [1,2]) {
  test(`Restore words review captured candidates before any effect at ${zoom*100}%`,async()=>{
    const directory=await mkdtemp('/private/tmp/ic-own-restore-command-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
    try {
      const page=await fixture(app,zoom,'multiple');const picker=page.getByRole('complementary',{name:'Restore review',exact:true});
      await expect(picker).toBeVisible();await expect(page.locator('.application-command-error')).toContainText('Own display receipt unavailable');
      expect(await app.evaluate(()=>(globalThis as any).restoreCommandFixture.requests.length)).toBe(0);
      await capture(app,`choices-${zoom*100}`);await picker.getByRole('button',{name:'Hide',exact:true}).click();
      await page.getByRole('button',{name:'Retry opening action',exact:true}).click();await expect(picker).toBeVisible();
      await expect.poll(()=>app.evaluate(()=>(globalThis as any).restoreCommandFixture.command.state)).toBe('landed');
      const receipts=await app.evaluate(()=>(globalThis as any).restoreCommandFixture.receipts);expect(receipts).toHaveLength(2);expect(receipts[1]).toEqual(receipts[0]);
      await picker.getByRole('button',{name:'Review Restore Worker B',exact:true}).click();const sheet=page.getByRole('complementary',{name:'Restore Worker B',exact:true});await expect(sheet).toBeVisible();
      expect(await app.evaluate(()=>(globalThis as any).restoreCommandFixture.requests.length)).toBe(0);
      await sheet.getByRole('button',{name:'Restore',exact:true}).click();await expect(sheet.getByText('Worker B restored. Held requests remain held.',{exact:true})).toBeVisible();
      const result=await app.evaluate(()=>(globalThis as any).restoreCommandFixture);expect(result.requests).toHaveLength(1);expect(result.requests[0].retirementId).toBe(result.entries[1].id);expect(result.requests[0].opId).not.toBe(result.presentation.operationId);
      expect(result.starts).toHaveLength(0);expect(result.state.utterances[0]).toMatchObject({state:'held',revision:4,text:'Own held work'});expect(result.state.workers[0].phase).toBe('held');
    } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
  });
  test(`a pending Restore never switches to a later retirement at ${zoom*100}%`,async()=>{
    const directory=await mkdtemp('/private/tmp/ic-own-restore-stale-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
    try {
      const page=await fixture(app,zoom,'single');const sheet=page.getByRole('complementary',{name:'Restore Worker A',exact:true});await expect(sheet).toBeVisible();
      await expect(page.locator('.application-command-error')).toContainText('Own display receipt unavailable');
      await app.evaluate(()=>{const data=(globalThis as any).restoreCommandFixture;data.state.pathRetirements=data.state.pathRetirements.map((entry:any)=>entry.track===data.entries[0].track?{...entry,id:'01960000-0000-5000-8000-000000000099'}:entry);data.state.sequence++;});
      await expect(sheet.getByText('This saved path is no longer set aside.',{exact:true})).toBeVisible();await expect(sheet.getByRole('button',{name:'Restore',exact:true})).toBeDisabled();
      await sheet.getByRole('button',{name:'Hide',exact:true}).click();await page.getByRole('button',{name:'Retry opening action',exact:true}).click();
      const picker=page.getByRole('complementary',{name:'Restore review',exact:true});await expect(picker).toBeVisible();await expect(picker.getByRole('button',{name:'Review Restore Worker A',exact:true})).toBeDisabled();
      await expect(picker.getByRole('button',{name:'Review Restore Worker B',exact:true})).toHaveCount(0);await capture(app,`stale-${zoom*100}`);
      const result=await app.evaluate(()=>(globalThis as any).restoreCommandFixture);expect(result.requests).toHaveLength(0);expect(result.presentation.restore[0].id).toBe(result.entries[0].id);
    } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
  });
  test(`held Restore words can open while held worker instructions stay stopped at ${zoom*100}%`,async()=>{
    const directory=await mkdtemp('/private/tmp/ic-own-restore-held-command-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
    try {
      const page=await fixture(app,zoom,'held');await expect(page.getByRole('button',{name:'Start request: Own held work',exact:true})).toBeDisabled();
      await page.getByRole('button',{name:'Start request: Restore the saved path',exact:true}).click();const sheet=page.getByRole('complementary',{name:'Restore Worker A',exact:true});await expect(sheet).toBeVisible();
      await expect.poll(()=>app.evaluate(()=>(globalThis as any).restoreCommandFixture.command.state)).toBe('landed');await capture(app,`held-${zoom*100}`);
      const result=await app.evaluate(()=>(globalThis as any).restoreCommandFixture);expect(result.starts).toHaveLength(1);expect(result.requests).toHaveLength(0);expect(result.state.utterances[0]).toMatchObject({state:'held',revision:4});
    } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
  });
  test(`an empty captured Restore list does not select a later path at ${zoom*100}%`,async()=>{
    const directory=await mkdtemp('/private/tmp/ic-own-restore-empty-command-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
    try {
      const page=await fixture(app,zoom,'empty');const picker=page.getByRole('complementary',{name:'Restore review',exact:true});await expect(picker.getByText('No set-aside paths were available for this request.',{exact:true})).toBeVisible();
      await expect(picker.getByRole('button',{name:/Review Restore Worker/})).toHaveCount(0);expect(await app.evaluate(()=>(globalThis as any).restoreCommandFixture.requests.length)).toBe(0);
    } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
  });
}
