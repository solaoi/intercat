import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

const pairId='01960000-0000-7000-8000-000000000011';
async function setup(options:{zoom?:number;phase?:string;lostEnd?:boolean;lostCommand?:boolean;wrongIdentity?:boolean;firstActive?:boolean;firstEmpty?:boolean;failOptions?:boolean;wrongOptions?:boolean;deferOptions?:boolean}={}) {
  const directory=await mkdtemp('/private/tmp/ic-two-ways-ui-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);await app.firstWindow();
  await app.evaluate(({ipcMain,BrowserWindow},options)=>{
    const checkpoint={turnId:'dummy-turn',sha:'a'.repeat(40),reference:'refs/intercat/dummy/checkpoint',unmergedPaths:[],liveProcesses:false};
    const word:any={id:'own-word',text:'Own paired dummy instruction',attachments:[],state:'placed',createdAt:Date.now(),revision:1,target:{mode:'lock',trackId:'A'},decision:{kind:'extend',trackId:'A',boundary:'when-told'},reason:null,editHold:null};
    const state:any={connected:true,daemonPid:77,sequence:1,workerConfigured:true,dispatchPaused:false,session:{id:'own-board',name:'Own dummy board'},utterances:[word],twoWays:[],branches:[],tracks:[{id:'A',label:'A',title:'Own original path',status:'waiting'},{id:'B',label:'B',title:'Own other path',status:'stopped'}],workers:[{trackId:'A',sessionId:'own-session',phase:'ready',identity:null,activeTurn:null,checkpointPending:false,checkpoint,approvals:[],prompts:[],output:null,detail:null}]};
    const fixture:any={state,ends:[],commands:[],reads:[],finishes:[],receipts:new Set(),lostEnd:options.lostEnd,lostCommand:options.lostCommand,failFinish:false,optionReads:[],optionWaiters:[],failOptions:options.failOptions};
    const make=(opId:string)=>({request:{opId,inputId:word.id,revision:word.revision,selection:{trackId:'A',sessionId:'own-session',variation:{modelId:'own-model',providerId:'meta',primary:'minimal',alternate:'xhigh'}}},revision:1,phase:options.phase??'waiting',sourceTrack:'A',sourceSession:'own-session',branchId:'own-branch',childInput:'own-child',point:options.firstEmpty?{kind:'initial',snapshot:{operation:'own-initial',sha:'b'.repeat(40),reference:'refs/intercat/own/initial'}}:{kind:'turn',checkpoint},variation:null,usageObservedAt:null,error:null,resume:null});
    const advance=(phase:string)=>{const job=state.twoWays[0];job.phase=phase;job.revision++;job.error=phase==='held'?'Own generated failure':null;word.twoWays=structuredClone(job);state.sequence++;};
    Object.assign(fixture,{advance});Object.assign(globalThis,{twoWaysFixture:fixture});
    for(const name of ['status','timeline-window','edit-request','two-ways-read','two-ways-command','two-ways-finish','two-ways-options'])ipcMain.removeHandler('intercat:'+name);
    ipcMain.handle('intercat:status',()=>structuredClone(state));
    ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
    ipcMain.handle('intercat:edit-request',(_event,action)=>{
      if(action.kind==='begin'){word.revision++;word.editHold={token:action.token,owner:'own-window',revision:word.revision};}
      if(action.kind==='apply'&&action.edit.kind==='readAs'){word.revision++;word.target={mode:'lock',trackId:action.edit.trackId};word.decision={kind:'schedule',trackId:action.edit.trackId,boundary:action.edit.boundary};word.editHold.revision=word.revision;}
      if(action.kind==='end'){
        fixture.ends.push(action);const key=JSON.stringify(action);
        if(!fixture.receipts.has(key)){
          if(action.edit.kind==='tryTwoWays'){state.twoWays=[make(action.edit.opId)];word.twoWays=structuredClone(state.twoWays[0]);}
          word.editHold=null;fixture.receipts.add(key);
          if(fixture.lostEnd&&action.edit.kind==='tryTwoWays'){fixture.lostEnd=false;throw new Error('Lost own paired acknowledgement');}
        }
      }
      state.sequence++;return structuredClone(word);
    });
    const selection={trackId:'A',sessionId:'own-session',variation:{modelId:'own-model',providerId:'meta',primary:'minimal',alternate:'xhigh'}};
    ipcMain.handle('intercat:two-ways-options',(_event,request)=>{fixture.optionReads.push(request);if(fixture.failOptions){fixture.failOptions=false;throw new Error('Own model choices unavailable');}const selected=word.decision.trackId==='B'?{...selection,trackId:'B',sessionId:'own-session-b',variation:{...selection.variation,alternate:'high'}}:selection;const value={inputId:options.wrongOptions?'another-word':request.inputId,revision:request.revision,selection:selected};return options.deferOptions?new Promise(resolve=>fixture.optionWaiters.push(()=>resolve(value))):value;});
    if(options.firstEmpty){state.workers[0].checkpoint=null;word.decision.kind='fork';}
    if(options.firstActive){state.workers[0].checkpoint=null;state.workers[0].activeTurn='own-first-turn';state.workers[0].phase='working';}
    ipcMain.handle('intercat:two-ways-read',(_event,request)=>{fixture.reads.push(request);return structuredClone(state.twoWays[0]);});
    ipcMain.handle('intercat:two-ways-command',(_event,request)=>{
      fixture.commands.push(request);if(fixture.lostCommand){fixture.lostCommand=false;throw new Error('Lost own command acknowledgement');}
      advance(request.action==='cancel'?'cancelling':request.action==='confirm-usage'?'inspecting-confirmed-usage':'inspecting');
      if(options.wrongIdentity)return {...state.twoWays[0],sourceSession:'someone-else'};
      return structuredClone(state.twoWays[0]);
    });
    ipcMain.handle('intercat:two-ways-finish',(_event,request)=>{fixture.finishes.push(request);if(fixture.failFinish){fixture.failFinish=false;throw new Error('Own receipt failure');}advance(state.twoWays[0].phase==='cancelling'?'cancelled':'queued');return structuredClone(state.twoWays[0]);});
    if(options.phase){state.twoWays=[make('01960000-0000-7000-8000-000000000011')];word.twoWays=structuredClone(state.twoWays[0]);if(options.phase==='held')advance('held');}
    const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(options.zoom??1);
  },options);
  const page=await app.firstWindow();await page.reload();
  return {app,page,cleanup:async()=>{await closeWindow(app);await rm(directory,{recursive:true,force:true});}};
}
const dialog=(page:Awaited<ReturnType<typeof setup>>['page'])=>page.getByRole('dialog',{name:'Try it two ways',exact:true});
const chip=(page:Awaited<ReturnType<typeof setup>>['page'])=>page.locator('[data-utterance-id="own-word"]');

for(const zoom of [1,2])test(`unknown usage waits for explicit confirmation at ${zoom*100}%`,async()=>{
  const {app,page,cleanup}=await setup({zoom,phase:'awaiting-usage-confirmation'});
  try{
    await chip(page).click();
    const modal=dialog(page);
    await expect(modal.getByRole('status')).toContainText('Plan usage is unavailable');
    const confirm=modal.getByRole('button',{name:'Run both · about twice the usage'});
    await expect(confirm).toBeEnabled();
    expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.commands)).toHaveLength(0);
    await confirm.focus();
    await expect(confirm).toBeFocused();
    await confirm.press('Enter');
    await expect(modal.getByRole('status')).toContainText('Rechecking plan usage');
    const commands=await app.evaluate(()=>(globalThis as any).twoWaysFixture.commands);
    expect(commands).toEqual([expect.objectContaining({id:pairId,action:'confirm-usage'})]);
  }finally{await cleanup();}
});

for(const zoom of [1,2])test(`paired entry saves with its edit lease and cancellation is keyboard reachable at ${zoom*100}%`,async({},info)=>{
  const {app,page,cleanup}=await setup({zoom});
  try{
    await page.getByRole('textbox',{name:'Request draft',exact:true}).fill('Own unsent draft stays here');
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});
    const entry=editor.getByRole('button',{name:/Try it two ways/});await expect(entry).toBeEnabled();
    await expect(editor.getByText(/Uses about twice the usage/)).toBeVisible();await expect(entry).toContainText('A′ at effort xhigh');await entry.focus();
    expect(await entry.evaluate(element=>{const r=element.getBoundingClientRect();return element.contains(document.elementFromPoint(r.left+r.width/2,r.top+r.height/2));})).toBe(true);
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    const entryImage=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));await writeFile(info.outputPath(`entry-${zoom}.png`),Buffer.from(entryImage,'base64'));
    await entry.press('Enter');
    const modal=dialog(page);await expect(modal).toBeVisible();await expect(modal.getByRole('status')).toContainText('Waiting for this instruction');
    await expect(modal.getByText(/Waiting for your Start on Worker A/)).toBeVisible();
    const cancel=modal.getByRole('button',{name:'Cancel two ways',exact:true});await cancel.focus();
    expect(await cancel.evaluate(element=>{const r=element.getBoundingClientRect();return element.contains(document.elementFromPoint(r.left+r.width/2,r.top+r.height/2));})).toBe(true);
    const image=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));await writeFile(info.outputPath(`two-ways-${zoom}.png`),Buffer.from(image,'base64'));
    await cancel.press('Enter');await expect(modal.getByRole('status')).toContainText('Cancelling');await expect(cancel).toHaveCount(0);
    await modal.getByRole('button',{name:'Close two-way preparation',exact:true}).click();await expect(chip(page)).toBeFocused();
    await expect(page.getByRole('textbox',{name:'Request draft',exact:true})).toHaveValue('Own unsent draft stays here');
    const calls=await app.evaluate(()=>(globalThis as any).twoWaysFixture.ends);expect(calls.filter((call:any)=>call.edit.kind==='tryTwoWays')).toHaveLength(1);
    await app.evaluate(()=>(globalThis as any).twoWaysFixture.advance('cancelled'));await expect(page.getByRole('button',{name:/Start request:/})).toBeVisible();
  }finally{await cleanup();}
});

test('a lost atomic entry acknowledgement retries exactly the same End and operation',async()=>{
  const {app,page,cleanup}=await setup({lostEnd:true});
  try{
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});await editor.getByRole('button',{name:/Try it two ways/}).click();
    await expect(editor.getByRole('status')).toContainText('Lost own paired acknowledgement');
    await editor.getByRole('button',{name:'Retry change',exact:true}).click();await expect(dialog(page)).toBeVisible();
    const ends=await app.evaluate(()=>(globalThis as any).twoWaysFixture.ends.filter((call:any)=>call.edit.kind==='tryTwoWays'));expect(ends).toHaveLength(2);expect(ends[0]).toEqual(ends[1]);
  }finally{await cleanup();}
});

test('lost control acknowledgements survive closing and retain their exact operation and revision',async()=>{
  const {app,page,cleanup}=await setup({phase:'held',lostCommand:true});
  try{
    await chip(page).click();const modal=dialog(page);await modal.getByRole('button',{name:'Retry preparation',exact:true}).click();await expect(modal.getByRole('alert')).toContainText('Lost own command');
    await modal.getByRole('button',{name:'Close two-way preparation',exact:true}).click();await chip(page).click();
    await modal.getByRole('button',{name:'Retry last action',exact:true}).click();await expect(modal.getByRole('status')).toContainText('Checking plan usage');
    const calls=await app.evaluate(()=>(globalThis as any).twoWaysFixture.commands);expect(calls).toHaveLength(2);expect(calls[0]).toEqual(calls[1]);
  }finally{await cleanup();}
});

test('reviewing saved state clears an uncertain control without creating a new action',async()=>{
  const {app,page,cleanup}=await setup({phase:'held',lostCommand:true});
  try{
    await chip(page).click();const modal=dialog(page);await modal.getByRole('button',{name:'Cancel two ways',exact:true}).click();await expect(modal.getByRole('alert')).toContainText('Lost own command');
    await modal.getByRole('button',{name:'Review saved state',exact:true}).click();await expect(modal.getByRole('button',{name:'Retry last action',exact:true})).toHaveCount(0);
    expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.commands)).toHaveLength(1);expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.reads)).toHaveLength(1);
  }finally{await cleanup();}
});

test('receipt confirmation failures preserve the branch and reconnect exposes the same saved request',async()=>{
  const {app,page,cleanup}=await setup({phase:'awaiting-receipt'});
  try{
    await app.evaluate(()=>(globalThis as any).twoWaysFixture.failFinish=true);await chip(page).click();const modal=dialog(page);
    await modal.getByRole('button',{name:'Confirm preparation',exact:true}).click();await expect(modal.getByRole('alert')).toContainText('Own receipt failure');
    await modal.getByRole('button',{name:'Confirm preparation',exact:true}).click();await expect(modal.getByRole('status')).toContainText('Both instructions are queued');
    expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.finishes)).toEqual([{id:pairId},{id:pairId}]);expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.ends)).toHaveLength(0);
  }finally{await cleanup();}
});

test('responses from another source session are rejected and global stop still permits cancellation',async()=>{
  const {app,page,cleanup}=await setup({phase:'held',wrongIdentity:true});
  try{
    await app.evaluate(()=>{const f=(globalThis as any).twoWaysFixture;f.state.dispatchPaused=true;f.state.sequence++;});await chip(page).click();const modal=dialog(page);
    await expect(modal.getByRole('button',{name:'Retry preparation',exact:true})).toBeDisabled();await expect(modal.getByRole('button',{name:'Cancel two ways',exact:true})).toBeEnabled();
    await modal.getByRole('button',{name:'Cancel two ways',exact:true}).click();await expect(modal.getByRole('alert')).toContainText('another instruction');
  }finally{await cleanup();}
});

test('a cancelled ordinary instruction uses a destination card without a fictitious original question',async()=>{
  const {app,page,cleanup}=await setup();
  try{
    await app.evaluate(()=>{const f=(globalThis as any).twoWaysFixture;f.state.utterances[0].state='orphaned';f.state.utterances[0].reason='The paired branch preparation was cancelled. Choose another destination.';f.state.sequence++;});
    const card=page.getByRole('group',{name:'Instruction needs a destination',exact:true});await expect(card).toBeVisible();await expect(card.getByText('Original question',{exact:true})).toHaveCount(0);await expect(card.getByRole('combobox',{name:'Instruction destination',exact:true})).toBeVisible();
  }finally{await cleanup();}
});

test('a cancellation still waiting for history protection exposes its confirmation and can finish',async()=>{
  const {app,page,cleanup}=await setup({phase:'cancelling'});
  try{
    await chip(page).click();const modal=dialog(page);await expect(modal.getByRole('button',{name:'Confirm cancellation',exact:true})).toBeEnabled();
    await modal.getByRole('button',{name:'Confirm cancellation',exact:true}).click();
    await expect(modal.getByRole('status')).toContainText('Two-way preparation cancelled');
    expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.finishes)).toEqual([{id:pairId}]);
  }finally{await cleanup();}
});

test('the running first turn can prepare its next instruction with the advertised effort',async()=>{
  const {app,page,cleanup}=await setup({firstActive:true});
  try{
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});const entry=editor.getByRole('button',{name:/Try it two ways/});
    await expect(entry).toContainText('A′ at effort xhigh');await expect(entry).toBeEnabled();await entry.click();await expect(dialog(page)).toBeVisible();
    const ends=await app.evaluate(()=>(globalThis as any).twoWaysFixture.ends);expect(ends.find((value:any)=>value.edit.kind==='tryTwoWays').edit.selection.variation.alternate).toBe('xhigh');
  }finally{await cleanup();}
});

test('unavailable model choices require an explicit reread before a paired request can be saved',async()=>{
  const {app,page,cleanup}=await setup({failOptions:true});
  try{
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});const entry=editor.getByRole('button',{name:/Try it two ways/});
    await expect(editor.getByRole('alert')).toContainText('Own model choices unavailable');await expect(entry).toBeDisabled();
    await editor.getByRole('button',{name:'Read effort choices again',exact:true}).click();await expect(entry).toContainText('A′ at effort xhigh');await expect(entry).toBeEnabled();
    expect(await app.evaluate(()=>(globalThis as any).twoWaysFixture.optionReads)).toHaveLength(2);
  }finally{await cleanup();}
});

test('model choices for another instruction never enable the entry',async()=>{
  const {page,cleanup}=await setup({wrongOptions:true});
  try{
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});await expect(editor.getByRole('alert')).toContainText('another instruction');await expect(editor.getByRole('button',{name:/Try it two ways/})).toBeDisabled();
  }finally{await cleanup();}
});

test('a late choice read cannot replace options for the newly edited destination',async()=>{
  const {app,page,cleanup}=await setup({deferOptions:true});
  try{
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});
    await expect.poll(async()=>app.evaluate(()=>(globalThis as any).twoWaysFixture.optionReads.length)).toBe(1);
    await editor.getByRole('combobox',{name:'Edited request destination',exact:true}).selectOption('B');
    await expect.poll(async()=>app.evaluate(()=>(globalThis as any).twoWaysFixture.optionReads.length)).toBe(2);
    await app.evaluate(()=>(globalThis as any).twoWaysFixture.optionWaiters[1]());
    const entry=editor.getByRole('button',{name:/Try it two ways/});await expect(entry).toContainText('B′ at effort high');
    await app.evaluate(()=>(globalThis as any).twoWaysFixture.optionWaiters[0]());
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    await expect(entry).toContainText('B′ at effort high');await expect(entry).not.toContainText('xhigh');
  }finally{await cleanup();}
});

for(const zoom of [1,2])test(`the first Fork instruction offers a paired effort before any turn at ${zoom*100}%`,async()=>{
  const {app,page,cleanup}=await setup({zoom,firstEmpty:true});
  try{
    await chip(page).click();const editor=page.getByRole('dialog',{name:'Request',exact:true});
    const entry=editor.getByRole('button',{name:/Try it two ways/});await expect(entry).toBeEnabled();await entry.click();
    const modal=dialog(page);await expect(modal).toBeVisible();await expect(modal.getByText(/Before the first step/)).toBeVisible();
    const end=await app.evaluate(()=>(globalThis as any).twoWaysFixture.ends.find((call:any)=>call.edit.kind==='tryTwoWays'));
    expect(end.edit.selection.variation.alternate).toBe('xhigh');
  }finally{await cleanup();}
});
