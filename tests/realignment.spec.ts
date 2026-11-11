import { _electron as electron,expect,test } from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';

for(const zoom of [1,2])test(`realignment decisions retain their owner and saved choice at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-realignment-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const a='01a1a210-0000-7000-8000-000000000001',b='01a1a210-0000-7000-8000-000000000002';
      const tracks=[a,b].map((id,index)=>({id,label:index?'B':'A',title:`Dummy worker ${index?'B':'A'}`,status:'working',included:true,checkpointCount:1,lastClip:1,checkpoint:{sha:'a'.repeat(40)}}));
      const integration={id:'01a1a210-0000-7000-8000-000000000003',revision:4,status:'done',sessionName:'Dummy realignment',slug:'dummy',project:{id:'dummy-folder',name:'Dummy folder',root:'/fixture',path:'/fixture',commonDir:'/fixture/.git',base:'a'.repeat(40),baseBranch:'main'},error:null,tracks,messages:{},keptSecrets:[],contributions:{},assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0,pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions:[],plan:{undoConflict:null,targetRef:'refs/heads/intercat/dummy',tree:'d'.repeat(40),rows:tracks.map(track=>({track:track.id,state:'included',changes:[],conflicts:[]}))},candidate:{revision:4,state:'checked',buildRevision:4,workspace:'/private/tmp/dummy-only',head:'c'.repeat(40),tree:'d'.repeat(40),settingsHash:'f'.repeat(64),commits:[],blockedCommit:null,undo:null,undoConflict:null,secretScans:[],check:{command:null,code:null,stdout:'',stderr:'',tree:'d'.repeat(40),settingsHash:'f'.repeat(64)},reason:null,failedTrack:null,hooksSkipped:false,repair:null,dependencyRuns:[]}};
      const workers=[{trackId:a,sessionId:'dummy-work-a',activeTurn:null,phase:'ready',checkpoint:{sha:'a'.repeat(40)},checkpointPending:false,approvals:[],prompts:[]},{trackId:b,sessionId:'dummy-work-b',activeTurn:'dummy-turn-b',phase:'working',checkpoint:null,checkpointPending:false,approvals:[],prompts:[]}];
      const state={connected:true,sequence:1,workerConfigured:true,tracks,workers,integrations:[integration],realignments:[] as any[],pendingRealignments:[] as any[],utterances:[],workspaces:[],project:{id:'dummy-project',name:'Dummy',folders:[integration.project]},trackFolders:{[a]:'dummy-folder',[b]:'dummy-folder'},repairRuns:[],editor:{phase:'ready',accepting:true},privacy:{mode:'basic',revision:0}};
      const fixture={state,requests:[] as any[],controls:[] as any[],saved:new Map<string,any>(),stopLost:false};Object.assign(globalThis,{realignmentFixture:fixture});
      const inspection={processes:[{identity:{pid:99999,started_seconds:1,started_micros:1},app:'Code',files:1,workingDirectory:false,fingerprint:'1'.repeat(64)}],unreadable:0,fingerprint:'2'.repeat(64)};
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:integration-command');ipcMain.handle('intercat:integration-command',()=>integration);
      ipcMain.removeHandler('intercat:realign-request');ipcMain.handle('intercat:realign-request',(_event,request)=>{
        fixture.requests.push(request);if(request.trackId!==a||request.source.kind!=='commit'||request.source.jobId!==integration.id||request.source.revision!==4)throw new Error('Wrong dummy realignment');
        const job={request,sessionId:'dummy-work-a',revision:1,phase:'needs-processes',keepProcesses:false,waitForAccess:false,stopCommand:null,stopCheckpoint:null,inspection:null,error:null};state.realignments=[job];state.sequence++;return job;
      });
      ipcMain.removeHandler('intercat:realign-control');ipcMain.handle('intercat:realign-control',(_event,request)=>{
        fixture.controls.push(request);const job=state.realignments[0];
        if(fixture.saved.has(request.opId)){if(JSON.stringify(fixture.saved.get(request.opId))!==JSON.stringify(request))throw new Error('A saved choice changed');return job;}
        if(request.id!==job.request.opId||request.revision!==job.revision)throw new Error('Stale dummy choice');
        fixture.saved.set(request.opId,request);job.revision++;
        switch(request.action.kind){
          case 'keepProcesses':job.keepProcesses=true;job.phase='waiting-processes';break;
          case 'stopProcesses':job.phase='needs-access';job.inspection=inspection;state.sequence++;if(!fixture.stopLost){fixture.stopLost=true;throw new Error('Dummy acknowledgement lost');}break;
          case 'wait':job.waitForAccess=true;job.phase='waiting-access';break;
          case 'goAhead':if(request.action.fingerprint!==inspection.fingerprint)throw new Error('Wrong file-access scope');job.phase='preparing';state.pendingRealignments=[{operationId:'dummy-file-operation',request:job.request,state:'dispatched'}];break;
          case 'retry':job.phase='preparing';break;
          default:throw new Error('Unexpected dummy choice');
        }
        state.sequence++;return job;
      });
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();await page.reload();
    async function releaseCard(){await page.getByRole('textbox',{name:'Request draft',exact:true}).focus();await page.mouse.move(5,5);}
    await page.getByRole('button',{name:'Commit',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Commit review',exact:true});
    await expect(sheet.getByRole('button',{name:'Realign A',exact:true})).toBeEnabled();
    await sheet.getByRole('button',{name:'Realign A',exact:true}).click();
    await expect(sheet).toHaveCount(0);
    const card=page.locator('[data-attention-card^="realign-"]:not([inert])');
    await expect(card.getByText('Worker A still has running processes',{exact:true})).toBeVisible();
    await card.getByRole('button',{name:'Keep (realign waits)',exact:true}).click();await releaseCard();
    await expect(card.getByRole('button',{name:'Keep (realign waits)',exact:true})).toBeDisabled();
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeEnabled();
    await card.getByRole('button',{name:'Stop it',exact:true}).click();await releaseCard();
    await expect(card.getByText('Dummy acknowledgement lost',{exact:true})).toBeVisible();
    await card.getByRole('button',{name:'Retry saved choice',exact:true}).click();await releaseCard();
    await expect(card.getByText('Code has files open in A’s worktree',{exact:true})).toBeVisible();
    const saved=await app.evaluate(()=>{
      const fixture=(globalThis as any).realignmentFixture;const stop=fixture.controls.filter((r:any)=>r.action.kind==='stopProcesses');return{stop,worker:fixture.state.workers[1]};
    });
    expect(saved.stop).toHaveLength(2);expect(saved.stop[0]).toEqual(saved.stop[1]);
    expect(saved.worker.sessionId).toBe('dummy-work-b');expect(saved.worker.activeTurn).toBe('dummy-turn-b');
    await card.getByRole('button',{name:'Wait',exact:true}).click();await releaseCard();await expect(card.getByRole('button',{name:'Wait',exact:true})).toBeDisabled();
    await page.reload();await expect(card.getByRole('button',{name:'Wait',exact:true})).toBeDisabled();
    await expect(card.getByRole('button',{name:'Go ahead',exact:true})).toBeEnabled();
    const buttons=await card.locator('.realignment-actions button').evaluateAll(elements=>elements.map(element=>{const r=element.getBoundingClientRect();return{text:element.textContent,left:r.left,right:r.right,top:r.top,bottom:r.bottom};}));
    const viewport=page.viewportSize()??await page.evaluate(()=>({width:innerWidth,height:innerHeight}));
    for(const button of buttons){expect(button.left).toBeGreaterThanOrEqual(0);expect(button.right).toBeLessThanOrEqual(viewport.width+1);expect(button.bottom).toBeLessThanOrEqual(viewport.height+1);}
    const heading=await card.locator('.realignment strong').evaluate(element=>{const title=element.getBoundingClientRect();const scroll=element.closest('.desk-scroll, .stage-cards')?.getBoundingClientRect();return{top:title.top,bottom:title.bottom,scrollTop:scroll?.top??0,scrollBottom:scroll?.bottom??innerHeight};});
    expect(heading.top).toBeGreaterThanOrEqual(heading.scrollTop-1);expect(heading.bottom).toBeLessThanOrEqual(heading.scrollBottom+1);
    const output=path.join(process.cwd(),'research/verify/m4-realign-ui-20261109-01');await mkdir(output,{recursive:true});
    const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
    await writeFile(path.join(output,`access-${zoom*100}.png`),Buffer.from(png,'base64'));
    await card.getByRole('button',{name:'Go ahead',exact:true}).click();await releaseCard();
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeDisabled();
    await app.evaluate(()=>{const state=(globalThis as any).realignmentFixture.state;state.realignments[0].phase='held';state.realignments[0].revision++;state.realignments[0].error='Dummy interrupted file update';state.sequence++;});
    await expect(card.getByRole('button',{name:'Continue realignment',exact:true})).toBeVisible();
    await expect(card.getByRole('button',{name:'Discard realignment',exact:true})).toBeDisabled();
    await card.getByRole('button',{name:'Continue realignment',exact:true}).click();await releaseCard();
    await app.evaluate(()=>{const state=(globalThis as any).realignmentFixture.state;state.realignments[0].phase='done';state.realignments[0].revision++;state.pendingRealignments=[];state.sequence++;});
    await expect(card).toHaveCount(0);
    const final=await app.evaluate(()=>{const fixture=(globalThis as any).realignmentFixture;return{requests:fixture.requests,controls:fixture.controls,workers:fixture.state.workers};});
    expect(final.requests).toHaveLength(1);expect(final.controls.map((request:any)=>request.action.kind)).toEqual(['keepProcesses','stopProcesses','stopProcesses','wait','goAhead','retry']);
    expect(final.workers[0].sessionId).toBe('dummy-work-a');expect(final.workers[0].activeTurn).toBeNull();expect(final.workers[1].activeTurn).toBe('dummy-turn-b');
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
