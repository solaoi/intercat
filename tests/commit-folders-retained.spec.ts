import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';
import type {IntegrationJob,IntegrationRequest} from '../src/commit/types';

for(const zoom of [1,2])test(`folder switching retains drafts, diffs, and independent Check results at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-retained-reviews-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const jobs:IntegrationJob[]=['A','B'].map((letter,index)=>{
        const track=`dummy-track-${letter}`,tree=(index?'b':'a').repeat(40);
        const changes=[{path:`dummy-${letter}.txt`,before:null,after:{object:tree,mode:0o100644}}];
        return{id:`dummy-review-${letter}`,revision:1,status:index?'failed':'checked',slug:`dummy-${letter}`,sessionName:'Dummy multi-folder review',project:{id:`folder-${letter}`,name:`Dummy ${letter}`,root:`/dummy/${letter}`,path:`/dummy/${letter}`,commonDir:`/dummy/${letter}/.git`,base:'c'.repeat(40),baseBranch:'main'},
          tracks:[{id:track,label:letter,title:`Dummy work ${letter}`,included:true,checkpointCount:1,lastClip:1,checkpoint:{sha:tree}}],
          candidate:{revision:1,buildRevision:1,state:index?'check-failed':'checked',head:tree,tree,settingsHash:'d'.repeat(64),workspace:'/dummy/private',commits:[{track,sha:tree,parent:'c'.repeat(40),tree,message:`Dummy message ${letter}`,author:'Dummy',changes}],blockedCommit:null,undo:null,undoConflict:null,secretScans:[],check:{command:`printf DUMMY_${letter}`,code:index?7:0,stdout:`DUMMY_${letter}`,stderr:'',truncated:false},reason:index?'Dummy Check failed':null,failedTrack:index?track:null,hooksSkipped:false,dependencyRuns:[],repair:null,conflict:null},
          plan:{undoConflict:null,targetRef:`refs/heads/intercat/dummy-${letter}`,tree,rows:[{track,state:'included',changes,conflicts:[]}]},error:index?'Dummy Check failed':null,messages:{[track]:`Dummy message ${letter}`},keptSecrets:[],contributions:{},pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions:[],assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0};
      });
      const state:any={connected:true,sequence:1,workerConfigured:true,project:{id:'dummy-project',name:'Dummy folders',folders:jobs.map(job=>job.project)},integrations:jobs,trackFolders:Object.fromEntries(jobs.map(job=>[job.tracks[0].id,job.project.id])),tracks:jobs.flatMap(job=>job.tracks.map(track=>({...track,status:'done'}))),workers:[],workspaces:[],utterances:[],repairRuns:[]};
      const fixture={state,calls:[] as IntegrationRequest[]};Object.assign(globalThis,{retainedReviews:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:integration-command');ipcMain.handle('intercat:integration-command',(_event,request:IntegrationRequest)=>{
        fixture.calls.push(request);const action=request.action;
        if(action.action!=='editMessage')throw new Error('Switching a saved review must not re-open or rebuild it');
        const job=jobs.find(job=>job.id===action.id)!;
        if(job.revision!==action.revision||!job.tracks.some(track=>track.id===action.track))throw new Error('Wrong saved review binding');
        job.messages[action.track]=action.message;job.candidate!.commits[0].message=action.message;job.revision++;job.candidate!.revision=job.revision;state.sequence++;return job;
      });
      ipcMain.removeHandler('intercat:integration-diff');ipcMain.handle('intercat:integration-diff',(_event,request)=>({path:request.path,before:'',after:`Agent-created ${request.path}`,redacted:false,binary:false,tooLarge:false}));
      BrowserWindow.getAllWindows()[0].setSize(1040,760);BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();const sheet=page.getByRole('complementary',{name:'Commit review',exact:true});
    const open=async(folder:string)=>{await page.getByLabel('More project actions',{exact:true}).click();await page.getByRole('button',{name:`Review Dummy ${folder}`,exact:true}).click();};
    const capture=async(name:string)=>{await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve(null)))));const footer=await sheet.locator('.commit-footer').evaluate(element=>({bottom:element.getBoundingClientRect().bottom,height:innerHeight}));expect(footer.bottom).toBeLessThanOrEqual(footer.height+1);const output=path.join(process.cwd(),'research/verify/m4-folder-reviews-ui-20261109-01');await mkdir(output,{recursive:true});const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));await writeFile(path.join(output,`${name}-${zoom*100}.png`),Buffer.from(png,'base64'));};
    await open('A');await expect(sheet.getByRole('region',{name:'Diff for dummy-A.txt',exact:true})).toBeVisible();
    await sheet.getByRole('tab',{name:'Message',exact:true}).click();
    await sheet.getByRole('textbox',{name:'Message for Worker A',exact:true}).fill('Dummy draft retained for A');
    await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-B');
    await sheet.getByRole('tab',{name:'Check',exact:true}).click();await expect(sheet.getByText('Check failed · exit 7',{exact:true})).toBeVisible();await expect(sheet.getByText('DUMMY_B',{exact:true})).toBeVisible();await capture('folder-b');
    await sheet.getByRole('combobox',{name:'Commit review folder',exact:true}).selectOption('folder-A');
    await sheet.getByRole('tab',{name:'Message',exact:true}).click();await expect(sheet.getByRole('textbox',{name:'Message for Worker A',exact:true})).toHaveValue('Dummy draft retained for A');
    await sheet.getByRole('button',{name:'Save message and check',exact:true}).click();
    await sheet.getByRole('tab',{name:'Check',exact:true}).click();await expect(sheet.getByText('Check passed',{exact:true})).toBeVisible();await expect(sheet.getByText('DUMMY_A',{exact:true})).toBeVisible();await capture('folder-a');
    await sheet.getByRole('button',{name:'Hide commit review',exact:true}).click();
    await open('B');await sheet.getByRole('tab',{name:'Check',exact:true}).click();await expect(sheet.getByText('Check failed · exit 7',{exact:true})).toBeVisible();
    await page.reload();await open('A');await sheet.getByRole('tab',{name:'Message',exact:true}).click();await expect(sheet.getByRole('textbox',{name:'Message for Worker A',exact:true})).toHaveValue('Dummy draft retained for A');
    const fixture=await app.evaluate(()=>(globalThis as any).retainedReviews);
    expect(fixture.calls).toHaveLength(1);expect(fixture.calls[0].action).toMatchObject({action:'editMessage',id:'dummy-review-A',track:'dummy-track-A',revision:1});
    expect(fixture.state.integrations[1]).toMatchObject({id:'dummy-review-B',revision:1,status:'failed',messages:{'dummy-track-B':'Dummy message B'},candidate:{check:{code:7,stdout:'DUMMY_B'}}});
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
