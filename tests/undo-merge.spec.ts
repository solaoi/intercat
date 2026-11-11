import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';

for(const zoom of [1,2])test(`undo owns its manual conflict and checks after a lost acknowledgement at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-undo-merge-ui-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const id='01a1a210-0000-7000-8000-000000000001',a='01a1a210-0000-7000-8000-000000000002',alt='01a1a210-0000-7000-8000-000000000003',b='01a1a210-0000-7000-8000-000000000004',merge='01a1a210-0000-7000-8000-000000000005';
      const tracks=[a,alt].map((id,index)=>({id,label:index?'A2':'A',title:'Own dummy branch',included:index===1,checkpointCount:3,lastClip:1,checkpoint:{sha:String(index+1).repeat(40)}}));
      const conflict={track:a,applied:['own-record-2'],remaining:['own-record-1'],base:'1'.repeat(40),target:'2'.repeat(40),incoming:'3'.repeat(40),paths:['parent.txt']};
      const candidate:any={revision:3,state:'conflict',head:'4'.repeat(40),tree:'4'.repeat(40),settingsHash:'5'.repeat(64),buildRevision:3,workspace:'/private/tmp/unused-own-undo-ui',commits:[],blockedCommit:null,undo:null,undoConflict:conflict,secretScans:[],check:null,reason:'The undo conflicts with a later edit.',failedTrack:null,hooksSkipped:false,conflict:null,dependencyRuns:[],repair:null};
      const job:any={id,revision:3,status:'merge-conflict',sessionName:'Own undo fixture',slug:'dummy',project:{id:'own-folder',name:'Own dummy repository',root:'/fixture',path:'/fixture',commonDir:'/fixture/.git',base:'1'.repeat(40),baseBranch:'main'},tracks,error:null,candidate,messages:{},keptSecrets:[],contributions:{},exclusions:[],assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0,
        pathReview:{families:[{root:a,members:tracks.map(t=>({id:t.id,label:t.label,checkpoint:{number:3,sha:t.checkpoint.sha},available:true})),forks:[{kind:'branch',parent:a,child:alt,checkpoint:{number:1,sha:'1'.repeat(40)},origin:{endedAt:1000,timelineAt:1000,turnNumber:1}}]}],choices:{[a]:{root:a,kept:alt,retired:{[a]:{kind:'fork',checkpoint:1}}}}},
        pathState:{pending:[],retired:{[a]:{kind:'fork',checkpoint:1}},undoRecords:2,ready:true,mixed:[],descendants:[]},retirements:[],plan:{undoConflict:conflict,targetRef:'refs/heads/intercat/dummy',tree:null,rows:tracks.map(t=>({track:t.id,state:t.included?'waiting':'excluded',changes:[],conflicts:[]}))}};
      const run:any={id:merge,revision:1,owner:{kind:'undo',id,revision:3},track:a,purpose:{kind:'merge'},phase:'manual',approvals:[],prompts:[],output:null,failure:null,session:null,turn:null,secretInput:null,refresh:null,restoring:null,remaining:null,externalEditor:false};
      const state:any={connected:true,sequence:1,session:{id:'own-session',name:'Own dummy'},project:{id:'own-project',name:'Own dummy',folders:[job.project]},tracks:[...tracks.map(t=>({...t,status:'done'})),{id:b,label:'B',title:'Own other work',status:'working'}],trackFolders:{[a]:job.project.id,[alt]:job.project.id,[b]:job.project.id},workers:[{trackId:b,sessionId:'own-b-session',activeTurn:'own-b-turn',phase:'working',checkpointPending:false,approvals:[],prompts:[]}],integrations:[job],repairRuns:[],utterances:[],workspaces:[],editor:{phase:'ready',accepting:true},privacy:{mode:'basic',revision:0}};
      const fixture={state,starts:[] as any[],applies:[] as any[],diffs:[] as any[],stops:[] as any[],lost:zoom===2,created:false,complete:()=>{
        job.status='checked';job.plan.tree='6'.repeat(40);job.candidate={...candidate,revision:4,state:'checked',undoConflict:null,check:{command:null,code:null,stdout:'',stderr:'',truncated:false},undo:{sha:'6'.repeat(40),parent:candidate.head,tree:'6'.repeat(40),author:'Intercat',message:'Undo A’s own dummy changes',records:['own-record-2','own-record-1'],changes:[{path:'independent.txt',before:{object:'1'.repeat(40),mode:33188},after:null},{path:'own-other.txt',before:null,after:{object:'2'.repeat(40),mode:33188}}],scan:{findings:[],error:null}}};state.sequence++;
      }};Object.assign(globalThis,{undoMergeFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:integration-command');ipcMain.handle('intercat:integration-command',()=>job);
      ipcMain.removeHandler('intercat:merge-start');ipcMain.handle('intercat:merge-start',(_event,request)=>{
        if(request.owner.kind!=='undo'||request.owner.id!==id||request.owner.revision!==3||request.track!==a||request.purpose.kind!=='merge')throw new Error('Wrong own dummy undo owner');
        fixture.starts.push(request);if(!fixture.created){fixture.created=true;state.repairRuns=[run];state.sequence++;}
        if(fixture.lost){fixture.lost=false;throw new Error('Own dummy saved undo acknowledgement lost');}return run;
      });
      ipcMain.removeHandler('intercat:merge-read');ipcMain.handle('intercat:merge-read',(_event,request)=>{
        if(request.id!==merge||request.revision!==run.revision)throw new Error('Wrong own dummy undo revision');
        return {id:merge,revision:run.revision,paths:['parent.txt'],file:{version:'own-version',editable:true,text:`<<<<<<< Intercat target ${merge}\nOwn later B edit\n=======\nOwn original base\n>>>>>>> Intercat change ${merge}\n`,sides:{path:'parent.txt',target:{object:'2'.repeat(40),mode:0o100644,text:'Own later B edit\n'},incoming:{object:'3'.repeat(40),mode:0o100644,text:'Own original base\n'}}}};
      });
      ipcMain.removeHandler('intercat:merge-apply');ipcMain.handle('intercat:merge-apply',(_event,request)=>{
        if(request.id!==merge||request.revision!==run.revision)throw new Error('Wrong own dummy undo resolution');
        fixture.applies.push(request);run.phase='done';run.revision++;job.revision++;job.status='building';job.candidate=null;job.plan.undoConflict=null;state.sequence++;return run;
      });
      ipcMain.removeHandler('intercat:integration-diff');ipcMain.handle('intercat:integration-diff',(_event,request)=>{fixture.diffs.push(request);if(request.owner.kind!=='undo'||!['independent.txt','own-other.txt'].includes(request.path))throw new Error('Wrong own dummy undo diff');return{path:request.path,before:request.path==='independent.txt'?'Own later A file\n':'',after:request.path==='own-other.txt'?'Own other Undo-only file\n':'',redacted:false,binary:false,tooLarge:false};});
      ipcMain.removeHandler('intercat:repair-stop');ipcMain.handle('intercat:repair-stop',(_event,request)=>{fixture.stops.push(request);run.phase='held';state.sequence++;return run;});
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();await page.reload();await page.getByRole('button',{name:'Commit',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Commit review',exact:true});
    const undo=sheet.getByRole('region',{name:'Resolve undo conflict',exact:true});
    await undo.getByRole('button',{name:'Let me resolve undo',exact:true}).click();
    if(zoom===2)await undo.getByRole('button',{name:'Retry saved undo request',exact:true}).click();
    const editor=undo.getByRole('region',{name:'Resolve Undo’s conflicts',exact:true});await expect(editor).toBeVisible();
    await expect(sheet.getByRole('radio',{name:/Keep Worker A2/})).toBeDisabled();
    const apply=editor.getByRole('button',{name:'Use resolution and check',exact:true});await expect(apply).toBeDisabled();
    await editor.getByRole('list',{name:'Unresolved hunks'}).getByRole('button',{name:'Take later edits',exact:true}).click();
    await sheet.getByRole('button',{name:'Hide commit review',exact:true}).click();
    await expect(sheet.getByRole('group',{name:'Unsaved merge editor drafts',exact:true})).toBeVisible();
    await sheet.getByRole('button',{name:'Keep editing',exact:true}).click();
    await apply.scrollIntoViewIfNeeded();await expect(apply).toBeInViewport();
    const proof=path.resolve('research/verify/m4-undo-merge-ui-20261109-01');await mkdir(proof,{recursive:true});
    const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
    await writeFile(path.join(proof,`undo-${zoom*100}.png`),Buffer.from(png,'base64'));
    await apply.click();await expect(editor).toHaveCount(0);
    await app.evaluate(()=>{(globalThis as any).undoMergeFixture.complete();});
    await expect(sheet.getByRole('button',{name:'Commit without check',exact:true})).toBeEnabled();
    await sheet.getByText('Undo commit · Intercat',{exact:true}).click();
    const review=sheet.locator('.commit-undo-review');
    await expect(review.getByLabel('Undo commit message',{exact:true})).toHaveText('Undo A’s own dummy changes');
    await expect(review.getByRole('region',{name:'Diff for independent.txt',exact:true})).toBeVisible();
    await review.getByRole('group',{name:'Undo changed files',exact:true}).getByRole('button',{name:'D independent.txt',exact:true}).scrollIntoViewIfNeeded();
    await expect(review).not.toContainText('Message for Worker');
    await review.scrollIntoViewIfNeeded();
    const reviewPng=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
    await writeFile(path.join(proof,`undo-review-${zoom*100}.png`),Buffer.from(reviewPng,'base64'));
    const result=await app.evaluate(()=>{const f=(globalThis as any).undoMergeFixture;return{starts:f.starts,applies:f.applies,diffs:f.diffs,workers:f.state.workers};});
    expect(result.starts).toHaveLength(zoom===2?2:1);if(zoom===2)expect(result.starts[0].opId).toBe(result.starts[1].opId);
    expect(result.diffs).toHaveLength(1);expect(result.diffs[0]).toMatchObject({owner:{kind:'undo'},path:'independent.txt',revision:4});
    expect(result.applies).toHaveLength(1);expect(result.applies[0].files[0].text).toBe('Own later B edit\n');
    await sheet.getByText('Undo commit · Intercat',{exact:true}).click();
    await app.evaluate(()=>{const f=(globalThis as any).undoMergeFixture;const job=f.state.integrations[0];job.status='failed';job.candidate.state='secret-found';job.candidate.undo.scan.findings=[{id:'own-dummy-finding',path:'independent.txt',line:1,endLine:1,rule:'own-dummy-rule'}];f.state.sequence++;});
    const secret=sheet.getByLabel('Secret findings',{exact:true});await expect(secret).toContainText('Undo commit');
    await secret.getByRole('button',{name:/independent.txt:1/}).click();
    await expect(review).toHaveAttribute('open','');
    await expect(review.getByRole('region',{name:'Diff for independent.txt',exact:true})).toBeVisible();
    await expect(secret.getByRole('button',{name:'Ask to remove',exact:true})).toBeDisabled();
    const other=review.getByRole('button',{name:'A own-other.txt',exact:true});await other.click();
    await expect(review.getByRole('region',{name:'Diff for own-other.txt',exact:true})).toBeVisible();
    await app.evaluate(()=>{const f=(globalThis as any).undoMergeFixture;f.state.integrations[0].finishedSinceOpen=1;f.state.sequence++;});
    await expect(sheet.getByText('1 newer checkpoint · Refresh to include',{exact:true})).toBeVisible();
    await expect(other).toHaveAttribute('aria-pressed','true');
    await app.evaluate(()=>{const f=(globalThis as any).undoMergeFixture;const run=f.state.repairRuns[0];run.phase='running';run.owner.revision=4;f.state.sequence++;});
    await expect(sheet.locator('.commit-target').getByRole('button',{name:'Refresh',exact:true})).toBeDisabled();
    await sheet.getByRole('button',{name:'Hide commit review',exact:true}).focus();await page.keyboard.press('Meta+.');
    await expect.poll(()=>app.evaluate(()=>(globalThis as any).undoMergeFixture.stops.length)).toBe(1);
    expect(await app.evaluate(()=>(globalThis as any).undoMergeFixture.stops[0].id)).toBe('01a1a210-0000-7000-8000-000000000005');
    expect(result.workers[0]).toMatchObject({sessionId:'own-b-session',activeTurn:'own-b-turn',phase:'working'});
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
