import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';

for(const source of ['commit','reparent','shared'] as const)for(const zoom of [1,2])test(`${source} realignment owns its manual merge and preserves other work at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-realign-merge-ui-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},{zoom,source})=>{
      const a='01a1a200-0000-7000-8000-000000000002',b='01a1a200-0000-7000-8000-000000000003',id='01a1a200-0000-7000-8000-000000000004',realign='01a1a200-0000-7000-8000-000000000005',shared='01a1a200-0000-5000-8000-000000000006';
      const tracks=[{id:a,label:'A',title:'Dummy realignment',status:'waiting'},{id:b,label:'B',title:'Other dummy work',status:'working'}];
      const job={request:{opId:realign,trackId:a,source:{kind:source==='shared'?'reparent':source,jobId:'dummy-published',revision:7,...source!=='commit'?{parent:b}:{}}},sessionId:'dummy-main-a',revision:5,phase:'needs-merge',mergeInput:source==='shared'?null:'dummy-input',mergeBoundary:null,sharedInput:source==='shared'?{id:shared,revision:3,track:b,label:'B',checkpoint:1}:null,resolutions:[],keepProcesses:false,waitForAccess:false,stopCommand:null,stopCheckpoint:null,inspection:null,error:null};
      const workers=[{trackId:a,sessionId:'dummy-main-a',activeTurn:null,phase:'held',checkpointPending:false,approvals:[],prompts:[]},{trackId:b,sessionId:'dummy-main-b',activeTurn:'dummy-main-b-turn',phase:'working',checkpointPending:false,approvals:[],prompts:[]}];
      const run={id,revision:3,owner:source==='shared'?{kind:'shared',id:shared,revision:3}:{kind:'realign',id:realign,revision:5},track:source==='shared'?b:a,purpose:{kind:'merge'},phase:'manual',approvals:[],prompts:[],output:null,failure:null,session:null,turn:null,secretInput:null,refresh:null,restoring:null,remaining:null,externalEditor:false};
      const state={connected:true,sequence:1,workerConfigured:true,tracks,workers,integrations:[],repairRuns:[] as typeof run[],realignments:[job],pendingRealignments:[],utterances:[],workspaces:[],editor:{phase:'ready',accepting:true},privacy:{mode:'basic',revision:0}};
      const fixture={state,starts:[] as any[],applies:[] as any[],stops:[] as any[]};Object.assign(globalThis,{realignMergeFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:merge-start');ipcMain.handle('intercat:merge-start',(_event,request)=>{
        if(request.owner.kind!==(source==='shared'?'shared':'realign')||request.owner.id!==(source==='shared'?shared:realign)||request.owner.revision!==(source==='shared'?3:5)||request.track!==(source==='shared'?b:a)||request.purpose.kind!=='merge')throw new Error('Wrong dummy merge owner');
        fixture.starts.push(request);state.repairRuns=[run];state.sequence++;return run;
      });
      ipcMain.removeHandler('intercat:repair-stop');ipcMain.handle('intercat:repair-stop',(_event,request)=>{if(request.id!==id)throw new Error('Wrong dummy Stop owner');fixture.stops.push(request);return run;});
      ipcMain.removeHandler('intercat:merge-read');ipcMain.handle('intercat:merge-read',(_event,request)=>{
        if(request.id!==id||request.revision!==run.revision)throw new Error('Wrong dummy merge revision');
        return {id,revision:run.revision,paths:['b.txt'],file:{version:'dummy-original-version',editable:true,text:`<<<<<<< Intercat target ${id}\ncommitted dummy change\n=======\nunsaved dummy work\n>>>>>>> Intercat change ${id}\n`,sides:{path:'b.txt',target:{object:'1'.repeat(40),mode:0o100644,text:'committed dummy change\n'},incoming:{object:'2'.repeat(40),mode:0o100644,text:'unsaved dummy work\n'}}}};
      });
      ipcMain.removeHandler('intercat:merge-apply');ipcMain.handle('intercat:merge-apply',(_event,request)=>{
        if(request.id!==id||request.revision!==run.revision)throw new Error('Wrong dummy resolution owner');
        fixture.applies.push(request);run.phase='done';run.revision++;job.phase='preparing';job.sharedInput=null;state.sequence++;return run;
      });
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
    },{zoom,source});
    const page=await app.firstWindow();await page.reload();
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeDisabled();
    await page.getByRole('button',{name:'Review realignment merge',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:source==='shared'?'Shared merge for B':'Realignment merge for A',exact:true});
    await expect(sheet).toBeVisible();await sheet.getByRole('button',{name:'Let me resolve',exact:true}).click();
    const editor=sheet.getByRole('region',{name:source==='shared'?'Resolve B’s conflicts':'Resolve A’s conflicts',exact:true});
    await sheet.focus();await page.keyboard.press('Meta+.');
    await expect.poll(()=>app.evaluate(()=>(globalThis as any).realignMergeFixture.stops.length)).toBe(1);
    await expect(editor.getByText(source==='shared'?'For the shared checkpoint':'For realignment',{exact:true})).toBeVisible();
    await expect(editor.getByRole('button',{name:source==='shared'?'Use shared resolution':'Use resolution and realign',exact:true})).toBeDisabled();
    await editor.getByRole('list',{name:'Unresolved hunks'}).getByRole('button',{name:source==='shared'?'Take B':'Take A',exact:true}).click();
    await sheet.getByRole('button',{name:source==='shared'?'Hide shared merge':'Hide realignment merge',exact:true}).click();
    await expect(sheet.getByRole('group',{name:'Unsaved realignment choices',exact:true})).toBeVisible();
    await sheet.getByRole('button',{name:'Keep editing',exact:true}).click();
    const apply=editor.getByRole('button',{name:source==='shared'?'Use shared resolution':'Use resolution and realign',exact:true});await apply.scrollIntoViewIfNeeded();await expect(apply).toBeInViewport();
    const geometry=await sheet.evaluate(element=>{const rect=element.getBoundingClientRect();return {left:rect.left,right:rect.right,width:innerWidth,height:innerHeight};});
    expect(geometry.left).toBeGreaterThanOrEqual(0);expect(geometry.right).toBeLessThanOrEqual(geometry.width+1);
    const proof=path.resolve('research/verify/m4-shared-merge-ui-20261109-01');await mkdir(proof,{recursive:true});
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    const captured=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
    await writeFile(path.join(proof,`${source}-${zoom*100}.png`),Buffer.from(captured,'base64'));
    await apply.click();await expect(editor).toHaveCount(0);
    const continued=source==='shared'?page.getByRole('complementary',{name:'Realignment merge for A',exact:true}):sheet;
    await expect(continued.getByText('Realignment will continue at the worker’s idle boundary.',{exact:true})).toBeVisible();
    const result=await app.evaluate(()=>(globalThis as any).realignMergeFixture);
    expect(result.starts).toHaveLength(1);expect(result.starts[0].owner).toMatchObject({kind:source==='shared'?'shared':'realign',revision:source==='shared'?3:5});
    expect(result.stops).toHaveLength(1);expect(result.stops[0].id).toBe(result.state.repairRuns[0].id);
    expect(result.applies).toHaveLength(1);expect(result.applies[0].files[0].text).toBe('unsaved dummy work\n');
    expect(result.state.workers[1]).toMatchObject({sessionId:'dummy-main-b',activeTurn:'dummy-main-b-turn',phase:'working'});
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
