import {_electron as electron,expect,test} from '@playwright/test';
import {mkdtemp,rm,writeFile} from 'node:fs/promises';
import {closeWindow} from './support/close';
import {mockDrafts} from './support/drafts';
import type {IntegrationJob,IntegrationRequest} from '../src/commit/types';
for(const kind of ['borrowedRetiredChanges','extendsCancelledContext'] as const)for(const zoom of [1,2])test(`saved branch choices block ${kind} and survive retry at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-path-choice-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try{
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},{zoom,kind})=>{
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);
      const tracks=['A','A2','B'].map((label,index)=>({id:`01a1a210-0000-7000-8000-00000000000${index+2}`,label,title:`Own dummy ${label} work`,included:true,checkpointCount:index===0?3:1,lastClip:1,checkpoint:{sha:String(index+1).repeat(40)}}));
      const [a,alt,b]=tracks.map(track=>track.id);
      const job:IntegrationJob={id:'01a1a210-0000-7000-8000-000000000001',revision:1,status:'choosing',sessionName:'Own path choice',slug:'dummy',project:{id:'own-folder',name:'Own dummy repository',root:'/fixture',path:'/fixture',commonDir:'/fixture/.git',base:'a'.repeat(40),baseBranch:'main'},tracks,error:null,candidate:null,messages:{},keptSecrets:[],contributions:{},exclusions:[],assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0,
        pathReview:{families:[{root:a,members:tracks.slice(0,2).map(track=>({id:track.id,label:track.label,checkpoint:{number:track.checkpointCount,sha:track.checkpoint.sha},available:true})),forks:[{kind:'branch',parent:a,child:alt,checkpoint:{number:1,sha:'1'.repeat(40)},origin:{endedAt:1794211200000,timelineAt:1000,turnNumber:1}}]}],choices:{}},
        pathState:{pending:[a],retired:{},undoRecords:0,ready:false,mixed:[],descendants:[]},retirements:[],plan:{undoConflict:null,targetRef:'refs/heads/intercat/dummy',tree:null,rows:tracks.map(track=>({track:track.id,state:'waiting',changes:[],conflicts:[]}))}};
      const state:any={connected:true,sequence:1,session:{id:'own-session',name:'Own dummy'},project:{id:'own-project',name:'Own dummy',folders:[job.project]},tracks:tracks.map(track=>({...track,status:'done'})),trackFolders:Object.fromEntries(tracks.map(track=>[track.id,job.project.id])),integrations:[job],utterances:[],workspaces:[],workers:[{trackId:a,sessionId:'own-worker-session',phase:'working',activeTurn:'own-active-turn',identity:null,detail:null,checkpointPending:false,checkpoint:null,prompts:[],approvals:[],output:null,secretRevision:null}],effects:[{id:'own-effect',trackId:a,sessionId:'own-worker-session',itemId:'own-item',turnId:'own-active-turn',clipId:null,kind:'external-write',target:'Own dummy effect',ruleId:'own-rule',outcome:'done',running:false,at:1000,reason:null}]};
      const memory={state,calls:[] as IntegrationRequest[],applied:new Set<string>(),choices:0,commits:0,lose:zoom===2};Object.assign(globalThis,{pathChoiceFixture:memory});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:integration-command');ipcMain.handle('intercat:integration-command',(_event,request:IntegrationRequest)=>{
        memory.calls.push(request);if(memory.applied.has(request.opId))return job;
        const action=request.action;
        if(action.action==='choosePath'){
          if(action.revision!==job.revision)throw new Error('The sheet changed · review again');
          const retired=action.track===a?alt:a;
          job.pathReview.choices[a]={root:a,kept:action.track,retired:{[retired]:action.track===a?{kind:'entireTrack'}:{kind:'fork',checkpoint:1}}};
          for(const track of job.tracks)if([a,alt].includes(track.id))track.included=track.id===action.track;
          job.pathState={pending:[],retired:job.pathReview.choices[a].retired,undoRecords:0,ready:action.track===a,mixed:action.track===a?[]:[{kind,source:a,dependency:kind==='borrowedRetiredChanges'?a:b,record:'own-published-record',track:b,after:1,through:3}],descendants:[]};
          job.revision++;job.status=action.track===a?'open':'choosing';job.plan!.tree=action.track===a?'b'.repeat(40):null;memory.choices++;state.sequence++;memory.applied.add(request.opId);
          if(memory.lose){memory.lose=false;throw new Error('Own dummy lost acknowledgement');}
        }else if(action.action==='prepare'){
          job.status='checked';job.candidate={revision:job.revision,state:'checked',head:'b'.repeat(40),tree:'b'.repeat(40),settingsHash:'c'.repeat(64),buildRevision:job.revision,workspace:'/private/tmp/own-unused-path-choice',commits:[],blockedCommit:null,undo:null,undoConflict:null,secretScans:[],check:{command:null,code:null,stdout:'',stderr:'',truncated:false},reason:null,failedTrack:null,hooksSkipped:false,conflict:null,dependencyRuns:[],repair:null};state.sequence++;
        }else if(action.action==='commit'){
          if(job.status!=='checked'||!job.pathState!.ready)throw new Error('Review its paths first');
          job.status='committing';job.retirements=[{id:'own-retirement',job:job.id,track:alt,label:'A2',kind:'aside',phase:'stopping',error:null}];state.sequence++;memory.commits++;
        }
        return job;
      });
    },{zoom,kind});
    const page=await app.firstWindow();await page.reload();
    await page.getByRole('button',{name:'Commit',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Commit review',exact:true});
    const paths=sheet.getByRole('region',{name:'Paths to keep'});
    await expect(paths).toBeVisible();
    await expect(paths).toContainText('Choose this path to include it.');
    await expect(paths).not.toContainText('This choice is saved in the review.');
    await expect(sheet.getByRole('button',{name:'Commit',exact:true})).toBeDisabled();
    await paths.getByRole('radio',{name:/Keep Worker A2/}).click();
    if(zoom===2)await sheet.getByRole('button',{name:'Retry action',exact:true}).click();
    await expect(paths).toContainText('running now · stops when you commit');
    await expect(paths).toContainText('effects stay');
    await expect(paths).toContainText('can’t replace A automatically');
    await expect(sheet.getByRole('checkbox',{name:'Include Worker A',exact:true})).toBeDisabled();
    const show=paths.getByRole('button',{name:'Show me',exact:true});await show.scrollIntoViewIfNeeded();await show.focus();await show.press('Enter');
    await expect(paths.getByText(new RegExp(`commit added ${kind==='borrowedRetiredChanges'?'A':'B'}’s saved changes after checkpoint 1, through checkpoint 3`))).toBeVisible();
    if(kind==='extendsCancelledContext')await expect(paths).toContainText('Its starting context was published with A’s commit and would also be undone.');
    const keep=paths.getByRole('button',{name:'Keep A',exact:true});await keep.scrollIntoViewIfNeeded();await expect(keep).toBeInViewport();
    const pixels=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));await writeFile(test.info().outputPath(`paths-${zoom}.png`),Buffer.from(pixels,'base64'));
    await keep.focus();await keep.press('Enter');
    await expect(paths.getByRole('radio',{name:/Keep Worker A2/})).not.toBeChecked();
    await expect(paths.getByRole('radio',{name:/Keep Worker A This/})).toBeChecked();
    const commit=sheet.getByRole('button',{name:'Commit without check',exact:true});await expect(commit).toBeEnabled();
    const before=await app.evaluate(()=>{const memory=(globalThis as any).pathChoiceFixture;return{choices:memory.choices,commits:memory.commits,calls:memory.calls};});
    expect(before.choices).toBe(2);expect(before.commits).toBe(0);
    const choices=before.calls.filter((call:IntegrationRequest)=>call.action.action==='choosePath');
    expect(choices.every((call:IntegrationRequest)=>Object.keys(call.action).sort().join(',')==='action,id,revision,track')).toBe(true);
    if(zoom===2)expect(choices[0].opId).toBe(choices[1].opId);
    await commit.scrollIntoViewIfNeeded();await commit.focus();await commit.press('Enter');
    await expect(paths).toContainText('Stopping the unchosen paths and saving their final checkpoints…');
    await expect(paths.getByRole('radio',{name:/Keep Worker A2/})).toBeDisabled();
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
