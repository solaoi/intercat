import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';
import type {IntegrationJob,IntegrationRequest} from '../src/commit/types';
import type {CommitGroup,CommitGroupRequest} from '../src/commit/group';

for(const zoom of [1,2])for(const mode of ['partial','empty'])test(`all-folder Commit ${mode} preserves candidate scope and saved retries at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-group-review-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},{zoom,empty})=>{
      const jobs:IntegrationJob[]=['A','B'].map((letter,index)=>{
        const track=`dummy-track-${letter}`,tree=(index?'b':'a').repeat(40);
        const changes=[{path:`dummy-${letter}.txt`,before:null,after:{object:tree,mode:0o100644}}];
        return{id:`dummy-review-${letter}`,revision:1,status:index?'failed':'checked',slug:`dummy-${letter}`,sessionName:'Dummy multi-folder review',project:{id:`folder-${letter}`,name:`Dummy ${letter}`,root:`/dummy/${letter}`,path:`/dummy/${letter}`,commonDir:`/dummy/${letter}/.git`,base:'c'.repeat(40),baseBranch:'main'},
          tracks:[{id:track,label:letter,title:`Dummy work ${letter}`,included:true,checkpointCount:1,lastClip:1,checkpoint:{sha:tree}}],
          candidate:{revision:1,buildRevision:1,state:index?'check-failed':'checked',head:tree,tree,settingsHash:'d'.repeat(64),workspace:'/dummy/private',commits:[{track,sha:tree,parent:'c'.repeat(40),tree,message:`Dummy message ${letter}`,author:'Dummy',changes}],blockedCommit:null,undo:null,undoConflict:null,secretScans:[],check:{command:`printf DUMMY_${letter}`,code:index?7:0,stdout:`DUMMY_${letter}`,stderr:'',truncated:false},reason:index?'Dummy Check failed':null,failedTrack:index?track:null,hooksSkipped:false,dependencyRuns:[],repair:null,conflict:null},
          plan:{undoConflict:null,targetRef:`refs/heads/intercat/dummy-${letter}`,tree,rows:[{track,state:'included',changes,conflicts:[]}]},error:index?'Dummy Check failed':null,messages:{[track]:`Dummy message ${letter}`},keptSecrets:[],contributions:{},pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions:[],assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0};
      });
      const state:any={connected:true,sequence:1,workerConfigured:true,project:{id:'dummy-project',name:'Dummy folders',folders:jobs.map(job=>job.project)},integrations:jobs,trackFolders:Object.fromEntries(jobs.map(job=>[job.tracks[0].id,job.project.id])),tracks:jobs.flatMap(job=>job.tracks.map(track=>({...track,status:'done'}))),workers:[],workspaces:[],utterances:[],repairRuns:[]};

      const group:CommitGroup={id:'dummy-group',revision:1,sessionId:'dummy-session',projectId:'dummy-project',status:'open',members:jobs.map((job,index)=>({folderId:job.project.id,name:job.project.name,jobId:empty&&index===1?null:job.id})),tracks:null,lastCommit:null,error:null};
      Object.assign(state,{commitGroups:[group]});
      const fixture={state,group,opens:[] as unknown[],calls:[] as IntegrationRequest[],commits:[] as CommitGroupRequest[],counts:[0,0]};Object.assign(globalThis,{groupReview:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:commit-group-open');ipcMain.handle('intercat:commit-group-open',(_event,request)=>{fixture.opens.push(request);if(empty&&fixture.opens.length===1)throw new Error('Dummy open response lost');return group;});
      ipcMain.removeHandler('intercat:commit-group-commit');ipcMain.handle('intercat:commit-group-commit',(_event,request:CommitGroupRequest)=>{
        fixture.commits.push(request);
        if(empty){if(request.targets.length!==1||request.targets[0].jobId!==jobs[0].id)throw new Error('An empty folder acquired a candidate');jobs[0].status='done';fixture.counts[0]++;group.status='done';group.revision++;state.sequence++;return group;}
        if(fixture.commits.length===1){
          if(jobs.some(job=>job.status!=='checked'))throw new Error('A Check has not passed');
          jobs[0].status='done';jobs[0].revision++;fixture.counts[0]++;
          jobs[1].status='commit-blocked';jobs[1].revision++;jobs[1].error='Dummy publication temporarily blocked';
          group.status='partial';group.revision++;group.lastCommit=request.opId;state.sequence++;
          throw new Error('Dummy connection lost after the saved group choice');
        }
        if(JSON.stringify(request)!==JSON.stringify(fixture.commits[0]))throw new Error('Retry changed the saved confirmation');
        return group;
      });
      ipcMain.removeHandler('intercat:integration-command');ipcMain.handle('intercat:integration-command',(_event,request:IntegrationRequest)=>{
        fixture.calls.push(request);const action=request.action;
        if(!('id' in action))throw new Error('Unexpected folder action');
        const job=jobs.find(job=>job.id===action.id)!;
        if(job.revision!==action.revision)throw new Error('Stale folder binding');
        if(action.action==='editMessage'){job.messages[action.track]=action.message;job.candidate!.commits[0].message=action.message;}
        else if(action.action==='recheck'){job.status='checked';job.candidate!.state='checked';job.candidate!.check!.code=0;job.error=null;}
        else if(action.action==='retryCommit'&&job===jobs[1]){job.status='done';fixture.counts[1]++;group.status='done';group.revision++;}
        else throw new Error('Unexpected folder action');
        job.revision++;job.candidate!.revision=job.revision;state.sequence++;return job;
      });
      ipcMain.removeHandler('intercat:integration-diff');ipcMain.handle('intercat:integration-diff',(_event,request)=>({path:request.path,before:'',after:`Agent-created ${request.path}`,redacted:false,binary:false,tooLarge:false}));
      BrowserWindow.getAllWindows()[0].setSize(1040,760);BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    },{zoom,empty:mode==='empty'});
    const page=await app.firstWindow(),sheet=page.getByRole('complementary',{name:'Commit review',exact:true});
    const capture=async(name:string)=>{await sheet.locator('.commit-scroll').evaluate(element=>{element.scrollTop=0;});await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve(null)))));const bounds=await sheet.locator('.commit-footer').evaluate(element=>({bottom:element.getBoundingClientRect().bottom,height:innerHeight}));expect(bounds.bottom).toBeLessThanOrEqual(bounds.height+1);const output=path.join(process.cwd(),'research/verify/m4-folder-group-ui-20261109-01');await mkdir(output,{recursive:true});const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));await writeFile(path.join(output,`${name}-${zoom*100}.png`),Buffer.from(png,'base64'));};
    await page.getByRole('button',{name:'Commit · 2 folders',exact:true}).click();
    const commit=sheet.getByRole('button',{name:'Commit all folders',exact:true});
    if(mode==='empty'){
      await expect(sheet.getByRole('alert')).toContainText('Dummy open response lost');
      await sheet.getByRole('button',{name:'Retry saved choice',exact:true}).click();
      await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-B');
      await expect(sheet.getByText('No saved work selected for this folder.',{exact:true})).toBeVisible();
      await expect(sheet.getByRole('textbox',{name:'Message for Worker A',exact:true})).toHaveCount(0);
      await expect(commit).toBeEnabled();await capture('empty');await commit.click();
      await expect(sheet.getByText('All selected folders committed',{exact:true})).toBeVisible();
      const result=await app.evaluate(()=>(globalThis as any).groupReview);
      expect(result.opens).toHaveLength(2);expect(result.opens[0]).toEqual(result.opens[1]);expect(result.counts).toEqual([1,0]);expect(result.commits[0].targets).toHaveLength(1);
      return;
    }
    await expect(sheet.getByRole('region',{name:'All-folder review'})).toContainText('Needs attention');
    await expect(commit).toBeDisabled();
    await sheet.getByRole('tab',{name:'Message',exact:true}).click();
    await sheet.getByRole('textbox',{name:'Message for Worker A',exact:true}).fill('Agent-created draft A');
    await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-B');
    await sheet.getByRole('tab',{name:'Check',exact:true}).click();
    await expect(sheet.getByText('Check failed · exit 7',{exact:true})).toBeVisible();
    await sheet.getByRole('button',{name:'Re-check',exact:true}).click();
    await expect(commit).toBeDisabled();
    await expect(sheet.getByRole('region',{name:'All-folder review'})).toContainText('Message draft');
    await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-A');
    await sheet.getByRole('tab',{name:'Message',exact:true}).click();
    await expect(sheet.getByRole('textbox',{name:'Message for Worker A',exact:true})).toHaveValue('Agent-created draft A');
    await sheet.getByRole('button',{name:'Save message and check',exact:true}).click();
    await expect(commit).toBeEnabled();await capture('ready');
    await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-B');
    await commit.click();
    await expect(sheet.getByRole('alert')).toContainText('Dummy connection lost');
    await expect(sheet.getByRole('textbox',{name:'Message for Worker B',exact:true})).toBeDisabled();
    await sheet.getByRole('button',{name:'Retry saved choice',exact:true}).click();
    await expect(sheet.getByRole('button',{name:'Commit remaining folders',exact:true})).toBeDisabled();
    await capture('partial');
    await sheet.getByRole('button',{name:'Hide commit review',exact:true}).click();
    await page.reload();await page.getByRole('button',{name:'Commit · 2 folders',exact:true}).click();
    await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-B');
    await sheet.getByRole('button',{name:'Retry Commit for Dummy B',exact:true}).click();
    await expect(sheet.getByText('All selected folders committed',{exact:true})).toBeVisible();await capture('done');
    const fixture=await app.evaluate(()=>(globalThis as any).groupReview);
    expect(fixture.counts).toEqual([1,1]);expect(fixture.commits).toHaveLength(2);expect(fixture.commits[1]).toEqual(fixture.commits[0]);
    expect(fixture.commits[0].targets).toEqual(fixture.state.integrations.map((job:IntegrationJob)=>({jobId:job.id,revision:2,head:job.candidate!.head,tree:job.candidate!.tree,settingsHash:job.candidate!.settingsHash})));
    expect(fixture.calls.map((request:IntegrationRequest)=>request.action.action)).toEqual(['recheck','editMessage','retryCommit']);
    expect(fixture.opens).toHaveLength(2);
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
