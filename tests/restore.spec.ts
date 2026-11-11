import {_electron as electron,expect,test,type ElectronApplication} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';

async function fixture(app:ElectronApplication,zoom:number,phase:'initial'|'merge'|'access') {
  await mockDrafts(app);
  await app.evaluate(({ipcMain,BrowserWindow},{zoom,phase})=>{
    const a='01a1a230-0000-7000-8000-000000000002',b='01a1a230-0000-7000-8000-000000000003',retirement='01a1a230-0000-5000-8000-000000000004',id='01a1a230-0000-7000-8000-000000000005',merge='01a1a230-0000-7000-8000-000000000006';
    const entry={id:retirement,job:'own-publication',publication:'own-published-operation',session:'own-session',folder:'own-folder',track:a,label:'A',kind:'aside',phase:'aside',error:null};
    const job:any={request:{opId:id,retirementId:retirement},trackId:a,sessionId:'own-session',folderId:'own-folder',revision:9,phase:phase==='merge'?'needs-merge':phase==='access'?'waiting-for-access':'waiting-for-processes',keepProcesses:false,error:null,inspection:phase==='access'?{processes:[{identity:{pid:1,started_seconds:1,started_micros:1},app:'Own dummy editor',files:2,workingDirectory:true,fingerprint:'own-file-holder'}],unreadable:1,fingerprint:'own-current-access'}:null,restored:null,mergeInput:phase==='merge'?{id,revision:2,track:a,label:'A',stage:'files',paths:['own.txt']}:null};
    const workers=[{trackId:a,sessionId:'own-main-a',activeTurn:null,phase:'held',checkpointPending:false,approvals:[],prompts:[]},{trackId:b,sessionId:'own-main-b',activeTurn:'own-main-b-turn',phase:'working',checkpointPending:false,approvals:[],prompts:[]}];
    const run:any={id:merge,revision:3,owner:{kind:'restore',id,revision:2},track:a,purpose:{kind:'merge'},phase:'manual',approvals:[],prompts:[],output:null,failure:null,session:null,turn:null,secretInput:null,refresh:null,restoring:null,remaining:null,externalEditor:false};
    const state:any={connected:true,sequence:1,workerConfigured:true,session:{id:'own-session',name:'Own Restore fixture'},tracks:[{id:a,label:'A',title:'Own retired work',status:'done'},{id:b,label:'B',title:'Own other work',status:'working'}],workers,integrations:[],repairRuns:[],realignments:[],pendingRealignments:[],pathRetirements:[entry],pathRestores:phase==='initial'?[]:[job],restoreFollowups:[],asideProcessStops:[],utterances:[{id:'own-held-word',revision:4,text:'Own held instruction',state:'held',createdAt:1,attachments:[],target:{mode:'lock',trackId:a},decision:{kind:'schedule',trackId:a,boundary:'when-told'},reason:null}],workspaces:[],editor:{phase:'ready',accepting:true},privacy:{mode:'basic',revision:0}};
    const fixture:any={state,job,entry,run,groups:1,processStop:null,requests:[],controls:[],access:[],stops:[],mergeStarts:[],mergeApplies:[],mergeStops:[],lost:true};Object.assign(globalThis,{restoreFixture:fixture});
    const handle=(name:string,body:(_event:unknown,request:any)=>unknown)=>{ipcMain.removeHandler('intercat:'+name);ipcMain.handle('intercat:'+name,body);};
    handle('status',()=>state);
    const start=new Date(2026,0,1,14,0,0).getTime();
    handle('timeline-window',()=>({revision:state.sequence,reset:true,clips:[{id:'own-saved-step',trackId:a,repairRunId:null,ordinal:1,sessionId:'own-main-a',turnId:'own-saved-turn',category:'step',label:'Own saved edit',kind:'edit',start:start+60000,end:start+180000,running:false,generationEnd:null,approximate:false,toolTicks:[]}],marks:[],removed:[],removedMarks:[]}));
    handle('aside-processes',(_event,request)=>{if(request.retirementId!==retirement)throw new Error('Wrong own process scope');return {retirementId:retirement,groups:fixture.groups,error:null,stop:fixture.processStop};});
    handle('path-restore',(_event,request)=>{if(request.retirementId!==retirement)throw new Error('Wrong own Restore');fixture.requests.push(request);job.request=request;state.pathRestores=[job];state.sequence++;if(fixture.lost){fixture.lost=false;throw new Error('Own dummy saved Restore acknowledgement lost');}return job;});
    handle('path-restore-control',(_event,request)=>{if(request.id!==job.request.opId||request.revision!==job.revision)throw new Error('Wrong own Restore revision');fixture.controls.push(request);if(request.action.kind==='keepProcesses')job.keepProcesses=true;else if(request.action.kind==='hold')job.phase='held';else if(request.action.kind==='retry')job.phase='queued';else if(request.action.kind==='dismissRealign')state.restoreFollowups=[];job.revision++;state.sequence++;return job;});
    handle('path-restore-access',(_event,request)=>{if(request.fingerprint!==job.inspection.fingerprint||request.request.opId!==job.request.opId)throw new Error('Wrong own access fingerprint');fixture.access.push(request);job.phase='queued';job.inspection=null;job.revision++;state.sequence++;return job;});
    handle('aside-process-stop',(_event,request)=>{if(request.retirementId!==retirement)throw new Error('Wrong own process Stop');fixture.stops.push(request);fixture.groups=0;fixture.processStop={record:{request,track:a,session:'own-main-a',error:null},state:'complete'};job.phase='queued';job.revision++;state.sequence++;if(fixture.lostStop){fixture.lostStop=false;throw new Error('Own dummy process Stop acknowledgement lost');}return fixture.processStop;});
    handle('merge-start',(_event,request)=>{if(request.owner.kind!=='restore'||request.owner.id!==job.request.opId||request.owner.revision!==2||request.track!==a)throw new Error('Wrong own private Restore merge');fixture.mergeStarts.push(request);state.repairRuns=[run];state.sequence++;return run;});
    handle('repair-stop',(_event,request)=>{if(request.id!==merge)throw new Error('Wrong own merge Stop');fixture.mergeStops.push(request);return run;});
    handle('merge-read',()=>({id:merge,revision:run.revision,paths:['own.txt'],file:{version:'own-original',editable:true,text:`<<<<<<< Intercat target ${merge}\nOwn target\n=======\nOwn held edit\n>>>>>>> Intercat change ${merge}\n`,sides:{path:'own.txt',target:{object:'1'.repeat(40),mode:0o100644,text:'Own target\n'},incoming:{object:'2'.repeat(40),mode:0o100644,text:'Own held edit\n'}}}}));
    handle('merge-apply',(_event,request)=>{if(request.id!==merge||request.revision!==run.revision)throw new Error('Wrong own merge result');fixture.mergeApplies.push(request);run.phase='done';job.phase='queued';job.mergeInput=null;job.revision++;state.sequence++;return run;});
    const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
  },{zoom,phase});
  const page=await app.firstWindow();await page.reload();return page;
}
async function capture(app:ElectronApplication,name:string){const root=path.resolve('research/verify/m4-restore-ui-20261110-01');await mkdir(root,{recursive:true});const bytes=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));await writeFile(path.join(root,name+'.png'),Buffer.from(bytes,'base64'));}

for(const zoom of [1,2])test(`ghost Restore preserves its request and scopes process Stop at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-own-restore-ui-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    const page=await fixture(app,zoom,'initial');
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeDisabled();
    await expect(page.getByRole('button',{name:'Start request: Own held instruction',exact:true})).toBeDisabled();
    await expect(page.getByRole('combobox',{name:'Request destination'}).locator('option').filter({hasText:'Only A'})).toHaveCount(0);
    const ghost=page.locator('.ghost-row');await expect(ghost).toHaveCount(1);expect(await ghost.evaluate(element=>element.getBoundingClientRect().height)).toBe(26);
    await expect(ghost.locator('.track-menu')).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Stop A’s remaining processes',exact:true})).toBeVisible();
    const restore=page.getByRole('button',{name:'Restore Worker A',exact:true});await restore.focus();await expect(restore).toHaveCSS('opacity','1');await capture(app,`ghost-${zoom*100}`);await restore.click();
    const sheet=page.getByRole('complementary',{name:'Restore Worker A',exact:true});await sheet.getByRole('button',{name:'Restore',exact:true}).click();
    await sheet.getByRole('button',{name:'Retry saved Restore request',exact:true}).click();
    const requests=await app.evaluate(()=>(globalThis as any).restoreFixture.requests);expect(requests).toHaveLength(2);expect(requests[0]).toEqual(requests[1]);
    await sheet.getByRole('button',{name:'Keep (Restore waits)',exact:true}).click();await expect(sheet.getByRole('button',{name:'Keep (Restore waits)',exact:true})).toBeDisabled();
    await sheet.getByRole('button',{name:'Stop it',exact:true}).click();await sheet.getByRole('button',{name:'Hold Restore',exact:true}).click();
    await expect(sheet.getByRole('button',{name:'Continue Restore',exact:true})).toBeVisible();await sheet.getByRole('button',{name:'Continue Restore',exact:true}).click();
    await app.evaluate(()=>{const f=(globalThis as any).restoreFixture;f.job.phase='done';f.job.revision++;f.state.pathRetirements=[];f.state.restoreFollowups=[{requestId:f.job.request.opId,revision:f.job.revision,track:f.job.trackId,label:'A',target:'a'.repeat(40),source:{kind:'commit',jobId:'own-current-publication',revision:6}}];f.state.sequence++;});
    await expect(sheet.getByText('Worker A restored. Held requests remain held.',{exact:true})).toBeVisible();await expect(page.locator('.ghost-row')).toHaveCount(0);
    await sheet.getByRole('button',{name:'Hide',exact:true}).click();await page.mouse.move(0,0);await expect(page.getByRole('button',{name:'Realign A',exact:true})).toBeVisible();await page.getByRole('button',{name:'Not now',exact:true}).click();
    const result=await app.evaluate(()=>(globalThis as any).restoreFixture);expect(result.stops).toHaveLength(1);expect(result.stops[0].retirementId).toBe(result.entry.id);expect(result.state.workers[1]).toMatchObject({phase:'working',activeTurn:'own-main-b-turn'});expect(result.state.workers[0]).toMatchObject({phase:'held',sessionId:'own-main-a'});expect(result.state.utterances[0]).toMatchObject({state:'held',revision:4,text:'Own held instruction'});expect(result.controls.at(-1).action).toMatchObject({kind:'dismissRealign',head:'a'.repeat(40)});
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
for(const zoom of [1,2])test(`Restore owns the manual merge and keyboard Stop at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-own-restore-merge-ui-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    const page=await fixture(app,zoom,'merge');await page.getByRole('button',{name:'Review Restore',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Restore Worker A',exact:true});await sheet.getByRole('button',{name:'Let me resolve',exact:true}).click();
    const editor=sheet.getByRole('region',{name:'Resolve A’s conflicts',exact:true});await expect(editor.getByText('For Restore',{exact:true})).toBeVisible();
    await sheet.focus();await page.keyboard.press('Meta+.');await expect.poll(()=>app.evaluate(()=>(globalThis as any).restoreFixture.mergeStops.length)).toBe(1);
    await editor.getByRole('list',{name:'Unresolved hunks'}).getByRole('button',{name:'Take A',exact:true}).click();
    await sheet.getByRole('button',{name:'Hide Restore',exact:true}).click();await expect(sheet.getByRole('group',{name:'Unsaved Restore choices',exact:true})).toBeVisible();await sheet.getByRole('button',{name:'Keep editing',exact:true}).click();
    const apply=editor.getByRole('button',{name:'Use resolution and restore',exact:true});await apply.scrollIntoViewIfNeeded();await expect(apply).toBeInViewport();await capture(app,`merge-${zoom*100}`);await apply.click();await expect(editor).toHaveCount(0);
    const result=await app.evaluate(()=>(globalThis as any).restoreFixture);expect(result.mergeStarts[0].owner).toEqual({kind:'restore',id:result.job.request.opId,revision:2});expect(result.mergeApplies[0].files[0].text).toBe('Own held edit\n');expect(result.state.workers[1].activeTurn).toBe('own-main-b-turn');
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
for(const zoom of [1,2])test(`Restore confirms the displayed file access at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-own-restore-access-ui-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    const page=await fixture(app,zoom,'access');await page.getByRole('button',{name:'Review Restore',exact:true}).click();const sheet=page.getByRole('complementary',{name:'Restore Worker A',exact:true});
    await expect(sheet.getByText('1 process could not be inspected.',{exact:true})).toBeVisible();await sheet.getByRole('button',{name:'Go ahead',exact:true}).scrollIntoViewIfNeeded();await capture(app,`access-${zoom*100}`);await sheet.getByRole('button',{name:'Go ahead',exact:true}).click();
    const result=await app.evaluate(()=>(globalThis as any).restoreFixture);expect(result.access).toHaveLength(1);expect(result.access[0].fingerprint).toBe('own-current-access');expect(result.access[0].request).toEqual(result.job.request);expect(result.state.workers[0].phase).toBe('held');
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});

for(const zoom of [1,2])test(`ghost retries its exact process Stop after a lost acknowledgement at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-own-ghost-stop-ui-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    const page=await fixture(app,zoom,'initial');await app.evaluate(()=>{(globalThis as any).restoreFixture.lostStop=true;});
    await page.getByRole('button',{name:'Stop A’s remaining processes',exact:true}).click();
    await page.getByRole('button',{name:'Retry Stop',exact:true}).click();
    await expect(page.getByRole('button',{name:'Retry Stop',exact:true})).toHaveCount(0);
    const result=await app.evaluate(()=>(globalThis as any).restoreFixture);expect(result.stops).toHaveLength(2);expect(result.stops[0]).toEqual(result.stops[1]);expect(result.state.pathRestores).toHaveLength(0);expect(result.state.workers[1].activeTurn).toBe('own-main-b-turn');
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
