import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for(const zoom of [1,2]) test(`Reviewer lane keeps overlapping reviews separate and cannot receive work at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-reviewer-lane-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);
      const now=Date.now();
      const common={repairRunId:null,sessionId:'dummy',turnId:null,kind:'read',generationEnd:null,approximate:false,toolTicks:[],running:false};
      const source={...common,id:'source-a',trackId:'A',ordinal:1,category:'step',label:'Saved math update',start:now-110000,end:now-90000};
      const first={...common,id:'review-1',trackId:'reviewer',ordinal:1,category:'review',label:'Review A · complete',start:now-90000,end:now-45000};
      const second={...common,id:'review-2',trackId:'reviewer',ordinal:2,category:'review',label:'Review A · complete',start:now-70000,end:now-25000};
      const state={connected:true,sequence:1,workers:[{trackId:'A',phase:'ready',sessionId:'dummy-worker',activeTurn:null,checkpointPending:false,checkpoint:null,detail:null,identity:null,output:null,prompts:[],approvals:[]}],tracks:[{id:'A',label:'A',title:'Dummy math',status:'done'}],utterances:[],reviewerLane:{id:'reviewer',label:'Reviewer',title:'Code review',status:'done',active:0,queued:0,jobs:[] as {id:string;revision:number;state:string}[]},timeline:{clock:{timelineMs:now,epochMs:now,monoMs:1,bootId:'dummy'},summaries:[]}};
      const requests:unknown[]=[];const stops:string[]=[];const reviewStops:unknown[]=[];Object.assign(globalThis,{reviewLaneFixture:{requests,stops,state,reviewStops}});
      ipcMain.removeHandler('intercat:review-stop');ipcMain.handle('intercat:review-stop',(_event,request)=>{reviewStops.push(request);state.reviewerLane.jobs=[];state.sequence++;return {};});
      ipcMain.removeHandler('intercat:worker-stop');ipcMain.handle('intercat:worker-stop',(_event,request)=>{stops.push(request.trackId);return {};});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',(_e,request)=>{requests.push(request);return {revision:state.sequence,reset:true,clips:[source,first,second].filter(clip=>request.tracks.includes(clip.trackId)),removed:[],marks:[],removedMarks:[]};});
      ipcMain.removeHandler('intercat:clip-detail');ipcMain.handle('intercat:clip-detail',(_e,id)=>id==='source-a'?{clip:source,items:[]}:{clip:id==='review-1'?first:second,items:[],reviewedClips:[{id:'source-a',label:'A:1'}],reviews:[{id:'job',revision:1,state:'complete',summary:'Checked the saved math change.',detail:null,retryable:false}],reviewNotes:[]});
    },zoom);
    const page=await app.firstWindow();await page.reload();
    const grid=page.getByRole('grid',{name:'Tracks',exact:true});await expect(grid).toBeVisible();await grid.focus();await grid.press('End');
    const first=page.getByRole('button',{name:'Open step Reviewer:1: Review A · complete',exact:true});
    const second=page.getByRole('button',{name:'Open step Reviewer:2: Review A · complete',exact:true});
    await expect(first).toBeAttached();await expect(second).toBeAttached();
    const one=await first.boundingBox(),two=await second.boundingBox();expect(one).not.toBeNull();expect(two).not.toBeNull();expect(one!.y+one!.height).toBeLessThanOrEqual(two!.y);
    await expect(page.getByRole('button',{name:'Actions for Worker Reviewer',exact:true})).toHaveCount(0);
    await expect(page.getByRole('combobox',{name:'Request destination'}).getByRole('option',{name:/Reviewer/})).toHaveCount(0);
    await second.click();await expect(page.getByRole('region',{name:'Reviewer notes',exact:true})).toContainText('Checked the saved math change.');
    await app.evaluate(()=>{const fixture=(globalThis as typeof globalThis & {reviewLaneFixture:{state:{sequence:number;reviewerLane:{jobs:{id:string;revision:number;state:string}[]}}}}).reviewLaneFixture;fixture.state.reviewerLane.jobs=[{id:'live-review',revision:4,state:'running'}];fixture.state.sequence++;});
    await expect(page.getByRole('button',{name:'Stop reviews',exact:true})).toBeVisible();
    if(zoom===1)await page.getByRole('button',{name:'Stop reviews',exact:true}).click();else {await grid.focus();await page.keyboard.press('Meta+Period');}
    await expect.poll(()=>app.evaluate(()=>(globalThis as typeof globalThis & {reviewLaneFixture:{reviewStops:unknown[]}}).reviewLaneFixture.reviewStops.length)).toBe(1);
    const stoppedReview=await app.evaluate(()=>(globalThis as typeof globalThis & {reviewLaneFixture:{reviewStops:{reviewId:string;revision:number}[]}}).reviewLaneFixture.reviewStops[0]);expect(stoppedReview.reviewId).toBe('live-review');expect(stoppedReview.revision).toBe(4);
    expect(await app.evaluate(()=>(globalThis as typeof globalThis & {reviewLaneFixture:{stops:string[]}}).reviewLaneFixture.stops)).toEqual([]);
    await page.getByRole('button',{name:'Open A:1',exact:true}).click();await expect(page.getByText('A:1 / Saved math update',{exact:true})).toBeVisible();
    await grid.focus();await page.keyboard.press('Meta+Period');
    await expect.poll(()=>app.evaluate(()=>(globalThis as typeof globalThis & {reviewLaneFixture:{stops:string[]}}).reviewLaneFixture.stops)).toEqual(['A']);
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    await writeFile(test.info().outputPath('reviewer-lane.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    const requests=await app.evaluate(()=>(globalThis as typeof globalThis & {reviewLaneFixture:{requests:{tracks:string[]}[]}}).reviewLaneFixture.requests);
    expect(requests.some(request=>request.tracks.includes('reviewer'))).toBeTruthy();
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
