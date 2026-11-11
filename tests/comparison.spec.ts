import {_electron as electron,expect,test,type ElectronApplication} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';
async function fixture(app:ElectronApplication,zoom:number,running:boolean,continued=false){
  await mockDrafts(app);
  await app.evaluate(({ipcMain,BrowserWindow},{zoom,running,continued})=>{
    const tracks=(continued?['A','A2','A3','A4','A5']:['A','A2']).map((label,index)=>({id:`01960000-0000-5000-8000-00000000000${index+1}`,label,title:`Own dummy ${label}`,included:true,checkpointCount:2,lastClip:1,checkpoint:{sha:String(index+1).repeat(40)}}));
    const [a,b]=tracks.map(track=>track.id),folder={id:'own-comparison-folder',name:'Own dummy repository',root:'/fixture',path:'/fixture',commonDir:'/fixture/.git',base:'a'.repeat(40),baseBranch:'main'};
    const job:any={id:'01960000-0000-7000-8000-000000000003',revision:1,status:'choosing',sessionName:'Own comparison',slug:'own',project:folder,tracks,error:null,candidate:null,messages:{},keptSecrets:[],contributions:{},exclusions:[],assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0,retirements:[],pathReview:{families:[{root:a,members:tracks.map(track=>({id:track.id,label:track.label,checkpoint:{number:2,sha:track.checkpoint.sha},available:true})),forks:[{kind:'branch',parent:a,child:b,checkpoint:{number:1,sha:'c'.repeat(40)},origin:{endedAt:1000,timelineAt:1000,turnNumber:1}}]}],choices:{}},pathState:{pending:[a],retired:{},undoRecords:0,ready:false,mixed:[],descendants:[]},plan:{undoConflict:null,targetRef:'refs/heads/intercat/own',tree:null,rows:tracks.map(track=>({track:track.id,state:'waiting',changes:[],conflicts:[]}))}};
    const compared=continued?[tracks[2],tracks[4]]:tracks;
    if(continued){
      const family=job.pathReview.families[0];
      for(const [parent,child] of [[tracks[0],tracks[2]],[tracks[1],tracks[3]],[tracks[3],tracks[4]]]){
        family.forks.push({kind:'continuation',parent:parent.id,child:child.id,checkpoint:{number:2,sha:parent.checkpoint.sha},origin:{endedAt:2000,timelineAt:2000,turnNumber:2}});
        family.members.find((member:any)=>member.id===parent.id).available=false;
      }
    }
    const state:any={connected:true,sequence:1,session:{id:'own-session',name:'Own comparison'},project:{id:'own-project',name:'Own dummy',folders:[folder]},tracks:tracks.map(track=>({...track,status:'done'})),trackFolders:{[a]:folder.id,[b]:folder.id},integrations:[job],comparisons:[],utterances:[],workspaces:[],workers:[]};
    const data:any={state,opens:[],controls:[],diffs:[],integration:[],value:null,lose:zoom===2,loseControl:zoom===2,applied:new Set(),evidenceFailure:false};Object.assign(globalThis,{comparisonFixture:data});
    const handle=(name:string,body:(_event:unknown,value:any)=>unknown)=>{ipcMain.removeHandler('intercat:'+name);ipcMain.handle('intercat:'+name,body);};
    handle('status',()=>state);handle('timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
    handle('integration-command',(_event,value)=>{data.integration.push(value);return job;});
    const save=()=>{state.comparisons=[data.value];state.sequence++;};
    handle('comparison-open',(_event,value)=>{
      data.opens.push(value);
      if(!data.value||data.value.id!==value.opId){data.value={id:value.opId,revision:1,attempt:1,request:value,session:state.session.id,folder:{id:folder.id,name:folder.name},settingsHash:'c'.repeat(64),reuseChecks:true,phase:running?'running':'done',summary:{label:running?'Checks haven’t run on both yet':`Only ${compared[1].label} passes the checks · Compare the diffs`,reason:null},sides:compared.map((track,index)=>({source:{point:{trackId:track.id,sha:track.checkpoint.sha},label:track.label,checkpoint:{sha:track.checkpoint.sha,turnId:'own-turn'}},tree:String(index+3).repeat(40),settingsHash:'c'.repeat(64),changes:[{path:'outcome.ts',before:null,after:{object:'a'.repeat(40),mode:33188}}],cached:false,outcome:running?null:{state:index?'passed':'failed',check:{tree:String(index+3).repeat(40),settingsHash:'c'.repeat(64),command:'Own dummy test command',code:index?0:1,stdout:'Own screened output\n',stderr:'',truncated:false},reason:null,dependencyRuns:[],dependencyError:null}}))};save();}
      if(JSON.stringify(value)!==JSON.stringify(data.value.request))throw new Error('Wrong saved own points');
      if(data.lose){data.lose=false;throw new Error('Own lost comparison acknowledgement');}return data.value;
    });
    handle('comparison-read',(_event,value)=>{if(value.id!==data.value.id)throw new Error('Wrong own comparison');if(data.delayNextRead){data.delayNextRead=false;const old=structuredClone(data.value);return new Promise(resolve=>{data.releaseRead=()=>resolve(old);});}return data.value;});
    handle('comparison-evidence',(_event,value)=>{
      if(value.id!==data.value.id)throw new Error('Wrong own evidence');if(data.evidenceFailure)throw new Error('Own evidence unavailable');
      return {id:value.id,sides:[{tests:{added:0,deleted:0,modified:0},skipAdded:0,runner:[],unreadable:[],commandUnparsed:false},{tests:{added:0,deleted:1,modified:0},skipAdded:1,runner:['package.json scripts / runner settings'],unreadable:[],commandUnparsed:false}],changes:[{path:'outcome.ts',before:{object:'a'.repeat(40),mode:33188},after:{object:'b'.repeat(40),mode:33188}}]};
    });
    handle('comparison-diff',(_event,value)=>{data.diffs.push(value);if(value.id!==data.value.id||value.basis!=='paths'||value.path!=='outcome.ts')throw new Error('Wrong own diff');return {path:value.path,before:'export const own = "Own parent value";\n',after:'export const own = "Own child value";\n',redacted:false,binary:false,tooLarge:false};});
    handle('comparison-control',(_event,value)=>{
      data.controls.push(value);if(data.applied.has(value.opId))return data.value;
      if(value.id!==data.value.id||value.revision!==data.value.revision||value.attempt!==data.value.attempt)throw new Error('Own stale control');
      data.value.phase=value.action==='hold'?'held':'running';data.value.revision++;if(value.action==='retry')data.value.attempt++;data.applied.add(value.opId);save();
      if(data.loseControl){data.loseControl=false;throw new Error('Own lost control acknowledgement');}return data.value;
    });
    const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
  },{zoom,running,continued});
  const page=await app.firstWindow();await page.reload();await page.getByRole('button',{name:'Commit',exact:true}).click();
  const sheet=page.getByRole('complementary',{name:'Commit review',exact:true});
  await expect(sheet.getByRole('radio',{name:continued?/Keep Worker A5/:/Keep Worker A2/})).not.toBeChecked();
  await sheet.getByRole('button',{name:continued?'Check both paths · A3 / A5':'Check both paths · A / A2',exact:true}).click();
  const dialog=page.getByRole('dialog',{name:'Compare saved paths',exact:true});await expect(dialog).toBeVisible();
  if(zoom===2){await expect(dialog).toContainText('Own lost comparison acknowledgement');await dialog.getByRole('button',{name:'Retry saved choice',exact:true}).click();}
  return {page,sheet,dialog};
}
async function capture(app:ElectronApplication,name:string){
  const root=path.resolve('research/verify/m4-comparison-ui-20261110-01');await mkdir(root,{recursive:true});
  const page=await app.firstWindow();await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
  const data=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));await writeFile(path.join(root,name+'.png'),Buffer.from(data,'base64'));
}
for(const zoom of [1,2]){
  test(`comparison uses the current writers of two paths after repeated Fresh at ${zoom*100}%`,async()=>{
    const root=await mkdtemp('/private/tmp/ic-own-continued-comparison-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+root]});
    try{
      const {sheet,dialog}=await fixture(app,zoom,false,true);
      await expect(dialog.getByRole('heading',{name:/Worker A3 Check failed/})).toBeVisible();
      await expect(dialog.getByRole('heading',{name:/Worker A5 Check passed/})).toBeVisible();
      const data=await app.evaluate(()=>(globalThis as any).comparisonFixture);
      for(const request of data.opens){
        expect(request.parent).toEqual({trackId:'01960000-0000-5000-8000-000000000003',sha:'3'.repeat(40)});
        expect(request.child).toEqual({trackId:'01960000-0000-5000-8000-000000000005',sha:'5'.repeat(40)});
      }
      await dialog.getByRole('button',{name:'Back to path choices',exact:true}).click();
      await expect(sheet.getByRole('radio')).toHaveCount(2);
      await expect(sheet.getByRole('radio',{name:/Keep Worker A3/})).not.toBeChecked();
      await expect(sheet.getByRole('radio',{name:/Keep Worker A5/})).not.toBeChecked();
      const compare=sheet.locator('.comparison-launch button');
      await expect(compare).toHaveCount(1);
      await expect(compare).toHaveText('Compare saved results · A3 / A5');
      await app.evaluate(()=>{
        const {state}= (globalThis as any).comparisonFixture;
        const track=state.tracks[4];track.checkpointCount=0;track.checkpoint=null;
        const member=state.integrations[0].pathReview.families[0].members[4];member.checkpoint=null;member.available=false;
        state.integrations[0].tracks[4].checkpointCount=0;state.integrations[0].tracks[4].checkpoint=null;state.sequence++;
      });
      await expect(compare).toBeDisabled();
      await expect(compare).toHaveText('Check both paths · A3 / A5');
      for(const choice of await sheet.getByRole('radio').all())await expect(choice).not.toBeChecked();
      await app.evaluate(()=>{
        const {state}=(globalThis as any).comparisonFixture,job=state.integrations[0];
        const family=job.pathReview.families[0],root=family.members[0],current=family.members[2];
        family.members=[root,current];family.forks=family.forks.filter((fork:any)=>fork.kind==='continuation'&&fork.parent===root.id);
        job.pathReview.choices={[root.id]:{root:root.id,kept:current.id,retired:{}}};job.pathState.pending=[];job.pathState.ready=true;state.sequence++;
      });
      await expect(sheet.getByRole('region',{name:'Paths to keep',exact:true})).toHaveCount(0);
    }finally{await closeWindow(app);await rm(root,{recursive:true,force:true});}
  });
  test(`saved path results and diffs never choose a passing path at ${zoom*100}%`,async()=>{
    const root=await mkdtemp('/private/tmp/ic-own-comparison-ui-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+root]});
    try{
      const {page,sheet,dialog}=await fixture(app,zoom,false);
      await expect(dialog).toContainText('Only A2 passes the checks');await expect(dialog).toContainText('Changes tests · 1 deleted, 1 skip added');await expect(dialog).toContainText('Changes how tests run · package.json scripts / runner settings');
      await expect(dialog.getByRole('heading',{name:/Worker A2 Check passed/})).toBeVisible();
      await expect(dialog.getByRole('button',{name:/Keep Worker/})).toHaveCount(0);await expect(dialog).not.toContainText('Either is fine');
      await capture(app,`results-${zoom*100}`);
      await dialog.getByRole('button',{name:'Compare the diffs',exact:true}).first().click();
      const diff=dialog.getByRole('region',{name:'Diff for outcome.ts',exact:true});await expect(diff).toBeVisible();await expect(diff.locator('.monaco-diff-editor')).toBeVisible();await diff.scrollIntoViewIfNeeded();
      await capture(app,`diff-${zoom*100}`);
      await dialog.press('Escape');await expect(dialog).not.toBeVisible();
      const saved=sheet.getByRole('button',{name:'Compare saved results · A / A2',exact:true});await expect(saved).toBeVisible();await expect(sheet.getByRole('radio',{name:/Keep Worker A2/})).not.toBeChecked();
      await saved.click();await expect(dialog).toContainText('Only A2 passes the checks');
      const data=await app.evaluate(()=>(globalThis as any).comparisonFixture);
      expect(data.integration.every((request:any)=>request.action.action==='open')).toBe(true);expect(data.diffs).toHaveLength(1);
      expect(new Set(data.opens.map((value:any)=>value.opId)).size).toBe(1);if(zoom===2)expect(data.opens[0]).toEqual(data.opens[1]);
      expect(data.opens[0].parent.sha).toBe('1'.repeat(40));expect(data.opens[0].child.sha).toBe('2'.repeat(40));
      await dialog.getByRole('button',{name:'Check with current settings',exact:true}).click();
      await expect.poll(()=>app.evaluate(()=>(globalThis as any).comparisonFixture.opens.length)).toBe(zoom===2?3:2);
      const checkedAgain=await app.evaluate(()=>(globalThis as any).comparisonFixture.opens);
      expect(checkedAgain.at(-1).opId).not.toBe(checkedAgain[0].opId);expect(checkedAgain.at(-1).parent).toEqual(checkedAgain[0].parent);expect(checkedAgain.at(-1).child).toEqual(checkedAgain[0].child);

    }finally{await closeWindow(app);await rm(root,{recursive:true,force:true});}
  });
  test(`saved checks stop and retry without losing their inputs at ${zoom*100}%`,async()=>{
    const root=await mkdtemp('/private/tmp/ic-own-comparison-held-ui-'),app=await electron.launch({args:['.','--preview','--user-data-dir='+root]});
    try{
      const {dialog}=await fixture(app,zoom,true);await expect(dialog).toContainText('Checks haven’t run on both yet');
      await dialog.getByRole('button',{name:'Stop checks',exact:true}).click();
      if(zoom===2){await expect(dialog).toContainText('Own lost control acknowledgement');await dialog.getByRole('button',{name:'Retry saved choice',exact:true}).click();}
      await expect(dialog.getByRole('button',{name:'Retry unfinished checks',exact:true})).toBeEnabled();await capture(app,`held-${zoom*100}`);
      await dialog.getByRole('button',{name:'Retry unfinished checks',exact:true}).click();await expect(dialog.getByRole('button',{name:'Stop checks',exact:true})).toBeEnabled();
      const data=await app.evaluate(()=>(globalThis as any).comparisonFixture);expect(data.value.attempt).toBe(2);expect(data.value.request).toEqual(data.opens[0]);
      const holds=data.controls.filter((value:any)=>value.action==='hold');expect(new Set(holds.map((value:any)=>value.opId)).size).toBe(1);if(zoom===2)expect(holds).toHaveLength(2);
      expect(data.integration.every((value:any)=>value.action.action==='open')).toBe(true);
      await app.evaluate(()=>{(globalThis as any).comparisonFixture.delayNextRead=true;});
      await expect.poll(()=>app.evaluate(()=>typeof (globalThis as any).comparisonFixture.releaseRead)).toBe('function');
      await dialog.getByRole('button',{name:'Stop checks',exact:true}).click();await expect(dialog.getByRole('button',{name:'Check with current settings',exact:true})).toBeEnabled();
      await dialog.getByRole('button',{name:'Check with current settings',exact:true}).click();
      await expect.poll(()=>app.evaluate(()=>(globalThis as any).comparisonFixture.value.id)).not.toBe(data.value.id);
      const newId=await app.evaluate(()=>(globalThis as any).comparisonFixture.value.id);await expect(dialog).toHaveAttribute('data-comparison-id',newId);
      await app.evaluate(()=>{(globalThis as any).comparisonFixture.releaseRead();});
      await dialog.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
      await expect(dialog).toHaveAttribute('data-comparison-id',newId);await expect(dialog.getByRole('button',{name:'Stop checks',exact:true})).toBeEnabled();

    }finally{await closeWindow(app);await rm(root,{recursive:true,force:true});}
  });
}
