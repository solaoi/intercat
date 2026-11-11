import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { requireVoiceOver } from './support/voiceover';
import { closeWindow } from './support/close';

async function setup(options:{zoom?:number;lostPrepare?:boolean;lostCommand?:boolean;phase?:string;wrongIdentity?:boolean}={}) {
  const directory=await mkdtemp('/private/tmp/ic-fresh-ui-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  await app.evaluate(({ipcMain,BrowserWindow},options)=>{
    const checkpoint={turnId:'dummy-turn',sha:'a'.repeat(40),reference:'refs/intercat/dummy/checkpoint',unmergedPaths:[],liveProcesses:false};
    const worker=(id:string,session=id+'-session')=>({trackId:id,sessionId:session,phase:'ready',identity:null,activeTurn:null,checkpointPending:false,checkpoint,output:'Own dummy result for '+id,approvals:[],prompts:[],detail:null});
    const track=(id:string)=>({id,label:id,title:'Generated dummy '+id,status:'stopped',completion:null});
    const state:any={connected:true,daemonPid:77,sequence:1,workerConfigured:true,dispatchPaused:false,timeline:{clock:{epochMs:100000,monoMs:100000,timelineMs:100000,bootId:'dummy-boot'},summaries:[]},session:{id:'dummy-board',name:'Generated board'},tracks:[track('A'),track('B')],workers:[worker('A'),worker('B')],workspaces:[],continuations:[],freshTracks:[],utterances:[],project:{id:'dummy-project',name:'Generated project',folders:[{id:'dummy-folder',root:'/private/tmp/generated-fresh-project',name:'Generated folder'}]},trackFolders:{A:'dummy-folder',B:'dummy-folder'}};
    const origin={turnNumber:1,endedAt:90000,timelineAt:90000};
    const fixture:any={state,prepares:[],commands:[],words:[],lostPrepare:options.lostPrepare,lostCommand:options.lostCommand,failWord:false,delayed:null,release:null};
    const advance=(phase:string)=>{state.freshTracks[0].phase=phase;state.freshTracks[0].revision++;state.sequence++;};
    const transfer=(source='A',successor='A2')=>{
      const job=state.freshTracks[0];if(source==='A')advance('transferred');
      state.continuations.push({operation:job.request.opId,source,successor,sourceSession:source+'-session',sessionId:successor+'-session',boardSession:'dummy-board',origin,ready:true,opened:true});
      state.tracks.find((value:any)=>value.id===source).status='done';state.workers.find((value:any)=>value.trackId===source).phase='done';
      state.tracks.push(track(successor));state.workers.push(worker(successor));state.trackFolders[successor]='dummy-folder';state.sequence++;
    };
    Object.assign(fixture,{advance,transfer});Object.assign(globalThis,{freshFixture:fixture});
    for(const name of ['status','timeline-window','fresh-prepare','fresh-command','fresh-read','stage','clip-detail','branch-points'])ipcMain.removeHandler('intercat:'+name);
    ipcMain.handle('intercat:status',()=>structuredClone(state));
    const clip={id:'dummy-step',trackId:'A',repairRunId:null,ordinal:1,sessionId:'A-session',turnId:'dummy-turn',category:'step',label:'Original saved dummy step',kind:'edit',start:60000,end:90000,running:false,generationEnd:null,approximate:false,toolTicks:[]};
    ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[clip],marks:[],removed:[],removedMarks:[]}));
    ipcMain.handle('intercat:clip-detail',()=>({clip,items:[]}));
    ipcMain.handle('intercat:branch-points',()=>({points:[{id:1,turnNumber:1,turnId:'dummy-turn',checkpoint:checkpoint.sha,endedAt:90000,timelineAt:90000,shellWasRunning:false,available:true}],before:null}));
    ipcMain.handle('intercat:fresh-prepare',async(_event,request)=>{
      fixture.prepares.push(request);
      const job={request,revision:1,phase:options.phase??'waiting-for-turn',successorTrack:'A2',successorSession:'A2-session',checkpoint,inspection:null,files:null,summaryReady:false,error:null};
      state.freshTracks=[job];state.sequence++;
      if(options.wrongIdentity)return {...job,request:{...request,sessionId:'another-session'}};
      if(fixture.lostPrepare){fixture.lostPrepare=false;state.freshTracks=[];throw new Error('Lost dummy prepare acknowledgement');}
      if(fixture.delayed)await new Promise<void>(resolve=>{fixture.release=resolve;});
      return structuredClone(job);
    });
    ipcMain.handle('intercat:fresh-command',(_event,request)=>{
      fixture.commands.push(request);
      if(fixture.lostCommand){fixture.lostCommand=false;throw new Error('Lost dummy command acknowledgement');}
      if(request.action.kind==='allow-access')advance('capturing-files');
      if(request.action.kind==='cancel')advance('cancelling');
      if(request.action.kind==='retry')advance('summarizing');
      return structuredClone(state.freshTracks[0]);
    });
    ipcMain.handle('intercat:fresh-read',()=>structuredClone(state.freshTracks[0]));
    ipcMain.handle('intercat:stage',(_event,request)=>{
      fixture.words.push(request);
      if(fixture.failWord){fixture.failWord=false;throw new Error('Lost dummy word acknowledgement');}
      return {id:request.opId,text:request.text,revision:1,state:'held',createdAt:1,attachments:[],target:request.target,decision:{kind:'extend',trackId:request.target.trackId,boundary:'now'}};
    });
    if(options.phase){const request={opId:'01960000-0000-7000-8000-000000000011',trackId:'A',sessionId:'A-session'};state.freshTracks=[{request,revision:4,phase:options.phase,successorTrack:'A2',successorSession:'A2-session',checkpoint,inspection:{fingerprint:'b'.repeat(64),processes:[{fingerprint:'dummy-open-app',app:'Generated editor',files:2}],unreadable:1},files:null,summaryReady:false,error:options.phase==='summary-held'?'Generated summary failure':null}];}
    const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(options.zoom??1);
  },options);
  const page=await app.firstWindow();await page.reload();
  return {app,page,cleanup:async()=>{await closeWindow(app);await rm(directory,{recursive:true,force:true});}};
}
const freshDialog=(page:Awaited<ReturnType<typeof setup>>['page'])=>page.getByRole('dialog',{name:'Start a fresh conversation',exact:true});

for(const zoom of [1,2])test(`fresh preparation and durable cancellation are readable by keyboard at ${zoom*100}%`,async({},info)=>{
  const {app,page,cleanup}=await setup({zoom});
  try {
    const entry=page.getByRole('button',{name:'Start fresh for Worker A',exact:true});await entry.focus();await entry.press('Enter');
    const dialog=freshDialog(page);await expect(dialog).toBeVisible();
    await expect(dialog.getByText('Worker A · Generated dummy A')).toBeVisible();
    await dialog.getByRole('button',{name:'Start fresh',exact:true}).click();
    await expect(dialog.getByRole('status')).toContainText('Waiting for the current step');
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    const pixels=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(info.outputPath('fresh-'+zoom+'.png'),Buffer.from(pixels,'base64'));
    const cancel=dialog.getByRole('button',{name:'Cancel start fresh',exact:true});
    expect(await cancel.evaluate(element=>{const r=element.getBoundingClientRect();return element.contains(document.elementFromPoint(r.left+r.width/2,r.top+r.height/2));})).toBe(true);
    await cancel.focus();await cancel.press('Enter');await expect(dialog.getByRole('status')).toContainText('Cancelling');
    await expect(dialog.getByRole('button',{name:'Cancel start fresh',exact:true})).toHaveCount(0);
    await dialog.getByRole('button',{name:'Close',exact:true}).click();
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeDisabled();
    await expect(page.getByRole('button',{name:'Review fresh conversation for Worker A',exact:true})).toBeFocused();
    await app.evaluate(()=>(globalThis as any).freshFixture.advance('cancelled'));
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeEnabled();
    expect(await app.evaluate(()=>(globalThis as any).freshFixture.commands)).toHaveLength(1);
  }finally{await cleanup();}
});

test('closing after a lost prepare acknowledgement reuses the same request and preserves the draft',async()=>{
  const {app,page,cleanup}=await setup({lostPrepare:true});
  try{
    await page.getByRole('textbox',{name:'Request draft',exact:true}).fill('Own unsent dummy request');
    await page.getByRole('button',{name:'Start fresh for Worker A',exact:true}).click();
    const dialog=freshDialog(page);await dialog.getByRole('button',{name:'Start fresh',exact:true}).click();
    await expect(dialog.getByRole('alert')).toContainText('Lost dummy prepare');
    await dialog.getByRole('button',{name:'Close fresh conversation',exact:true}).click();
    await page.getByRole('button',{name:'Start fresh for Worker A',exact:true}).click();
    await dialog.getByRole('button',{name:'Retry saving request',exact:true}).click();
    await expect(dialog.getByRole('status')).toContainText('Waiting');
    const calls=await app.evaluate(()=>(globalThis as any).freshFixture.prepares);expect(calls).toHaveLength(2);expect(calls[0]).toEqual(calls[1]);
    await page.keyboard.press('Escape');
    await expect(page.getByRole('textbox',{name:'Request draft',exact:true})).toHaveValue('Own unsent dummy request');
  }finally{await cleanup();}
});

test('the saved access card retries an identical action, then cancellation remains available while paused',async()=>{
  const {app,page,cleanup}=await setup({phase:'needs-access',lostCommand:true});
  try{
    await page.getByRole('button',{name:'Review start fresh',exact:true}).click();const dialog=freshDialog(page);
    await expect(dialog.getByRole('region',{name:'Current file access'})).toContainText('Generated editor · 2 open files');
    await dialog.getByRole('button',{name:'Go ahead',exact:true}).click();await expect(dialog.getByRole('alert')).toContainText('Lost dummy command');
    await page.keyboard.press('Escape');await page.getByRole('button',{name:'Review start fresh',exact:true}).click();
    await dialog.getByRole('button',{name:'Retry last action',exact:true}).click();await expect(dialog.getByRole('status')).toContainText('Saving the current files');
    const commands=await app.evaluate(()=>(globalThis as any).freshFixture.commands);expect(commands).toHaveLength(2);expect(commands[0]).toEqual(commands[1]);
    await app.evaluate(()=>{const f=(globalThis as any).freshFixture;f.state.dispatchPaused=true;f.state.sequence++;});
    await expect(dialog.getByText('All turns are paused. Resume from the main controls to continue.')).toBeVisible();
    await dialog.getByRole('button',{name:'Cancel start fresh',exact:true}).click();await expect(dialog.getByRole('status')).toContainText('Cancelling');
  }finally{await cleanup();}
});

test('a held summary recovers after renderer reload without creating another operation',async()=>{
  const {app,page,cleanup}=await setup({phase:'summary-held'});
  try{
    await page.reload();await page.getByRole('button',{name:'Review start fresh',exact:true}).click();
    const dialog=freshDialog(page);await expect(dialog.getByRole('alert')).toContainText('Generated summary failure');
    await dialog.getByRole('button',{name:'Retry start fresh',exact:true}).click();await expect(dialog.getByRole('status')).toHaveText('Making the handoff summary');
    expect(await app.evaluate(()=>(globalThis as any).freshFixture.prepares)).toEqual([]);
    expect((await app.evaluate(()=>(globalThis as any).freshFixture.commands))[0]).toMatchObject({id:'01960000-0000-7000-8000-000000000011',revision:4,action:{kind:'retry'}});
  }finally{await cleanup();}
});

test('a continuation redirects locked drafts, preserves old branching, and does not change an already prepared retry',async()=>{
  const {app,page,cleanup}=await setup();
  try{
    const destination=page.getByRole('combobox',{name:'Request destination',exact:true});await destination.selectOption('A');
    await page.getByRole('textbox',{name:'Request draft',exact:true}).fill('Own request locked to A');
    await page.getByRole('button',{name:'Start fresh for Worker A',exact:true}).click();const dialog=freshDialog(page);await dialog.getByRole('button',{name:'Start fresh',exact:true}).click();
    await app.evaluate(()=>(globalThis as any).freshFixture.transfer());
    await expect(dialog.getByRole('status')).toHaveText('Continued in a new conversation');await dialog.getByRole('button',{name:'Close fresh conversation',exact:true}).click();
    await expect(destination).toHaveValue('A2');await expect(page.getByText('A continued as A2 · this request goes there',{exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'Start Worker A',exact:true})).toBeDisabled();await expect(page.getByRole('button',{name:'Stop Worker A',exact:true})).toBeDisabled();
    await expect(page.locator('[data-worker-card="A2"]')).toContainText(/Continues A · context from .* · summarized/);
    await page.getByRole('gridcell',{name:/^B · Generated dummy B/}).click();await expect(destination).toHaveValue('A2');
    await app.evaluate(()=>{(globalThis as any).freshFixture.failWord=true;});
    await page.getByRole('button',{name:'Put',exact:true}).click();await expect(page.getByRole('button',{name:/^Retry save/})).toBeEnabled();
    await app.evaluate(()=>(globalThis as any).freshFixture.transfer('A2','A3'));
    await page.getByRole('button',{name:/^Retry save/}).click();
    const words=await app.evaluate(()=>(globalThis as any).freshFixture.words);expect(words).toHaveLength(2);expect(words[0]).toEqual(words[1]);expect(words[0].target).toEqual({mode:'lock',trackId:'A2'});expect(words[0].selectedTrackId).toBe('B');
    await destination.selectOption('A3');
    await page.getByRole('button',{name:'Open step A:1: Original saved dummy step',exact:true}).click();
    await page.getByRole('button',{name:'Branch at end of A’s turn 1',exact:true}).click();
    await expect(page.getByRole('dialog',{name:'Branch from a saved turn',exact:true})).toBeVisible();
  }finally{await cleanup();}
});

test('a mismatched conversation response is rejected without rendering it as saved',async()=>{
  const {page,cleanup}=await setup({wrongIdentity:true});
  try{await page.getByRole('button',{name:'Start fresh for Worker A',exact:true}).click();const dialog=freshDialog(page);await dialog.getByRole('button',{name:'Start fresh',exact:true}).click();await expect(dialog.getByRole('alert')).toContainText('another conversation');}finally{await cleanup();}
});

const nativeRun=promisify(execFile);
async function nativeFreshKey(app:Awaited<ReturnType<typeof setup>>['app'],code:number){
  expect(await app.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].isFocused())).toBe(true);
  await nativeRun('/usr/bin/osascript',['-e',`tell application "System Events" to key code ${code}`],{timeout:2500});
}
async function spokenFresh(app:Awaited<ReturnType<typeof setup>>['app'],expected:RegExp){
  let phrase='';
  await expect.poll(async()=>{
    expect(await app.evaluate(({BrowserWindow})=>BrowserWindow.getAllWindows()[0].isFocused())).toBe(true);
    phrase=(await nativeRun('/usr/bin/osascript',['-e','tell application "VoiceOver" to get content of last phrase'],{timeout:2500})).stdout.trim();
    return phrase;
  },{timeout:6000,intervals:[50,100,200]}).toMatch(expected);
  return phrase;
}
for(const zoom of [1,2])test(`Native VoiceOver reads Fresh controls and continuation at ${zoom*100}%`,async({},info)=>{
  test.skip(process.env.INTERCAT_NATIVE_FRESH_VOICEOVER!=='1','Requires VoiceOver and AppleScript control enabled by the user.');
  await requireVoiceOver();
  const {app,page,cleanup}=await setup({zoom});const phrases:{action:string;phrase:string}[]=[];
  try{
    await app.evaluate(({app,BrowserWindow})=>{const window=BrowserWindow.getAllWindows()[0];window.show();app.focus({steal:true});window.focus();});
    await page.getByRole('button',{name:'Start fresh for Worker A',exact:true}).focus();
    phrases.push({action:'source control',phrase:await spokenFresh(app,/Start fresh for Worker A/)});
    await nativeFreshKey(app,36);const dialog=freshDialog(page);await expect(dialog).toBeVisible();
    await dialog.getByRole('button',{name:'Start fresh',exact:true}).focus();
    phrases.push({action:'confirm fresh',phrase:await spokenFresh(app,/Start fresh/)});
    await nativeFreshKey(app,36);await expect(dialog.getByRole('status')).toContainText('Waiting');
    phrases.push({action:'saved progress',phrase:await spokenFresh(app,/Waiting for the current step and checkpoint/)});
    await dialog.getByRole('button',{name:'Close fresh conversation',exact:true}).focus();
    await nativeFreshKey(app,48);
    await expect(dialog.getByRole('button',{name:'Cancel start fresh',exact:true})).toBeFocused();
    phrases.push({action:'cancel control',phrase:await spokenFresh(app,/Cancel start fresh/)});
    await nativeFreshKey(app,36);await expect(dialog.getByRole('status')).toContainText('Cancelling');
    await expect(dialog.getByRole('status')).toBeFocused();
    phrases.push({action:'cancelling progress',phrase:await spokenFresh(app,/Cancelling and confirming/)});
    await dialog.getByRole('button',{name:'Close fresh conversation',exact:true}).focus();
    await nativeFreshKey(app,48);
    await expect(dialog.getByRole('button',{name:'Close',exact:true})).toBeFocused();
    phrases.push({action:'cancel saved',phrase:await spokenFresh(app,/^Close\b(?! fresh conversation)/)});
    await nativeFreshKey(app,36);await expect(dialog).toHaveCount(0);
    await app.evaluate(()=>(globalThis as any).freshFixture.advance('cancelled'));
    await page.getByRole('button',{name:'Start fresh for Worker A',exact:true}).click();
    await freshDialog(page).getByRole('button',{name:'Start fresh',exact:true}).click();
    await app.evaluate(()=>(globalThis as any).freshFixture.transfer());
    await expect(freshDialog(page).getByRole('status')).toHaveText('Continued in a new conversation');
    await freshDialog(page).getByRole('button',{name:'Close fresh conversation',exact:true}).click();
    const row=page.getByRole('gridcell',{name:/^A2 · Generated dummy A2/});
    await page.getByRole('grid',{name:'Tracks',exact:true}).focus();await nativeFreshKey(app,125);
    phrases.push({action:'summarized continuation',phrase:await spokenFresh(app,/Generated dummy A2.*summarized/)});
    await expect(row).toBeVisible();
  }finally{await writeFile(info.outputPath('fresh-voiceover.json'),JSON.stringify({zoom,phrases},null,2));await cleanup();}
});
