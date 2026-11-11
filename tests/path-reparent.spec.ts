import {_electron as electron,expect,test} from '@playwright/test';
import {mkdtemp,rm,writeFile} from 'node:fs/promises';
import {closeWindow} from './support/close';
import {mockDrafts} from './support/drafts';
import type {IntegrationJob,IntegrationRequest} from '../src/commit/types';
import type {RealignJob,RealignRequest} from '../src/commit/realign';
for(const zoom of [1,2])test(`dependent path realignment holds its review and resumes the new revision at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-path-reparent-');const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try{
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);
      const tracks=['A','A2','B'].map((label,index)=>({id:`01a1a220-0000-7000-8000-00000000000${index+2}`,label,title:`Own dummy ${label}`,included:index!==0,checkpointCount:1,lastClip:1,checkpoint:{sha:String(index+1).repeat(40)}}));const [a,alt,b]=tracks.map(track=>track.id);
      const job:IntegrationJob={id:'01a1a220-0000-7000-8000-000000000001',revision:5,status:'choosing',sessionName:'Own reparent test',slug:'dummy',project:{id:'own-folder',name:'Own dummy repository',root:'/fixture',path:'/fixture',commonDir:'/fixture/.git',base:'a'.repeat(40),baseBranch:'main'},tracks,error:null,candidate:null,messages:{},keptSecrets:[],contributions:{},exclusions:[],assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0,retirements:[],
        pathReview:{families:[{root:a,members:tracks.slice(0,2).map(track=>({id:track.id,label:track.label,checkpoint:{number:1,sha:track.checkpoint.sha},available:true})),forks:[{kind:'branch',parent:a,child:alt,checkpoint:{number:1,sha:'1'.repeat(40)},origin:{endedAt:1000,timelineAt:1000,turnNumber:1}}]}],choices:{[a]:{root:a,kept:alt,retired:{[a]:{kind:'fork',checkpoint:1}}}}},
        pathState:{pending:[],retired:{[a]:{kind:'fork',checkpoint:1}},undoRecords:1,ready:false,mixed:[],descendants:[{track:b,included:true,parents:[{track:a,checkpoint:{number:3,sha:'4'.repeat(40)},onto:alt}]}]},plan:{undoConflict:null,targetRef:'refs/heads/intercat/dummy',tree:null,rows:tracks.map(track=>({track:track.id,state:'waiting',changes:[],conflicts:[]}))}};
      const state:any={connected:true,sequence:1,session:{id:'own-session',name:'Own dummy'},project:{id:'own-project',name:'Own dummy',folders:[job.project]},tracks:tracks.map(track=>({...track,status:'done'})),trackFolders:Object.fromEntries(tracks.map(track=>[track.id,job.project.id])),integrations:[job],utterances:[],workspaces:[],realignments:[],workers:[{trackId:b,sessionId:'own-worker-b',phase:'working',activeTurn:'own-turn-b',identity:null,detail:null,checkpointPending:false,checkpoint:null,prompts:[],approvals:[],output:null,secretRevision:null}]};
      const memory={state,requests:[] as RealignRequest[],commands:[] as IntegrationRequest[],queued:null as RealignJob|null,applied:new Set<string>(),lose:zoom===2};Object.assign(globalThis,{pathReparentFixture:memory});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:realign-request');ipcMain.handle('intercat:realign-request',(_event,request:RealignRequest)=>{
        memory.requests.push(request);if(memory.applied.has(request.opId))return memory.queued;
        if(request.trackId!==b||request.source.kind!=='reparent'||request.source.jobId!==job.id||request.source.revision!==5||request.source.parent!==a)throw new Error('Wrong own dummy reparent source');
        memory.queued={request,sessionId:'own-worker-b',revision:1,phase:'waiting-for-worker',keepProcesses:false,waitForAccess:false,stopCommand:null,stopCheckpoint:null,inspection:null,error:null,mergeInput:null,mergeBoundary:null,sharedInput:null,resolutions:[]};
        state.realignments=[memory.queued];state.sequence++;memory.applied.add(request.opId);
        if(memory.lose){memory.lose=false;throw new Error('Own dummy lost reparent acknowledgement');}return memory.queued;
      });
      ipcMain.removeHandler('intercat:integration-command');ipcMain.handle('intercat:integration-command',(_event,request:IntegrationRequest)=>{
        memory.commands.push(request);
        if(request.action.action==='open'&&job.status==='preparing'){job.status='open';job.plan!.tree='b'.repeat(40);state.sequence++;}
        if(request.action.action==='prepare'){
          if(request.action.revision!==6)throw new Error('Only the updated own dummy review can build');
          job.status='checked';job.candidate={revision:6,state:'checked',head:'b'.repeat(40),tree:'b'.repeat(40),settingsHash:'c'.repeat(64),buildRevision:6,workspace:'/private/tmp/own-unused-reparent',commits:[],blockedCommit:null,undo:null,undoConflict:null,secretScans:[],check:{command:null,code:null,stdout:'',stderr:'',truncated:false},reason:null,failedTrack:null,hooksSkipped:false,conflict:null,dependencyRuns:[],repair:null};state.sequence++;
        }
        return job;
      });
    },zoom);
    const page=await app.firstWindow();await page.reload();await page.getByRole('button',{name:'Commit',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Commit review',exact:true});const paths=sheet.getByRole('region',{name:'Paths to keep'});
    const realign=paths.getByRole('button',{name:'Realign B onto A2',exact:true});await realign.scrollIntoViewIfNeeded();await realign.focus();await realign.press('Enter');
    if(zoom===2){const retry=paths.getByRole('button',{name:'Retry realignment request',exact:true});await expect(retry).toBeEnabled();await retry.click();}
    await expect(paths.getByRole('button',{name:'Realign B onto A2',exact:true})).toBeDisabled();
    await expect(sheet.getByRole('button',{name:'Commit',exact:true})).toBeDisabled();
    await expect(paths.getByRole('button',{name:'Leave B out of this commit',exact:true})).toBeDisabled();
    await sheet.getByRole('button',{name:'Hide commit review'}).click();await page.getByRole('button',{name:'Commit',exact:true}).click();
    await expect(paths.getByRole('button',{name:'Realign B onto A2',exact:true})).toBeDisabled();
    const before=await app.evaluate(()=>{const memory=(globalThis as any).pathReparentFixture;return{requests:memory.requests,commands:memory.commands};});
    expect(before.commands.every((call:IntegrationRequest)=>call.action.action==='open')).toBe(true);
    expect(new Set(before.requests.map((request:RealignRequest)=>request.opId)).size).toBe(1);
    await app.evaluate(()=>{const memory=(globalThis as any).pathReparentFixture;memory.queued.phase='done';memory.state.integrations[0].revision=6;memory.state.integrations[0].status='preparing';memory.state.integrations[0].pathState.descendants=[];memory.state.integrations[0].pathState.ready=true;memory.state.workers[0].activeTurn=null;memory.state.workers[0].phase='held';memory.state.sequence++;});
    const commit=sheet.getByRole('button',{name:'Commit without check',exact:true});await expect(commit).toBeEnabled();await commit.scrollIntoViewIfNeeded();
    const pixels=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));await writeFile(test.info().outputPath(`reparent-${zoom}.png`),Buffer.from(pixels,'base64'));
    const result=await app.evaluate(()=>{const memory=(globalThis as any).pathReparentFixture;return{session:memory.state.workers[0].sessionId,commands:memory.commands};});
    expect(result.session).toBe('own-worker-b');expect(result.commands.filter((call:IntegrationRequest)=>call.action.action==='prepare')).toHaveLength(1);expect(result.commands.some((call:IntegrationRequest)=>call.action.action==='commit')).toBe(false);
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
