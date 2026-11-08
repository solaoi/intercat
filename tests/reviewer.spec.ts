
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';

for (const zoom of [1, 2]) test(`Reviewer notes and idempotent retry at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-review-notes-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const window = BrowserWindow.getAllWindows()[0]; window.setSize(1040, 760); window.webContents.setZoomFactor(zoom);
      const now = Date.now();
      const clip = { id: 'clip-a', trackId: 'A', ordinal: 1, repairRunId: null, sessionId: 'dummy', turnId: 'dummy', category: 'step', title: 'Math update', label: 'Updated math.', kind: 'edit', start: now-100000, end: now-1000, generationEnd: now-90000, running: false, approximate: false, toolTicks: [] };
      const heldFix={state:'held',detail:'Worker stopped',inputId:'fix-input',inputRevision:2};
      const word={id:'fix-input',revision:2,text:'Internal dummy correction',createdAt:now,target:{mode:'lock',trackId:'A'},state:'held',attachments:[],decision:{kind:'schedule',trackId:'A',boundary:'after-turn'},reviewFix:{id:'fix-a',state:'held',path:'math.rs',clipId:'clip-a',current:true},reviewFixRunnable:true};
      const review = {id:'review-a', revision:1, state:'failed', summary:null, detail:'Review did not finish.'};
      const state = {connected:true, sequence:1, workers:[], tracks:[{id:'A',label:'A',title:'Dummy math',status:'done'}], utterances:[word], proposals:[], timeline:{clock:{timelineMs:now,epochMs:now,monoMs:1,bootId:'dummy'},summaries:[]}};
      Object.assign(globalThis,{reviewFixture:{state,review,requests:[] as unknown[]}});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:utterance-read');ipcMain.handle('intercat:utterance-read',()=>word);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[clip],removed:[],marks:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:clip-detail');ipcMain.handle('intercat:clip-detail',()=>({clip,items:[],reviews:[review],reviewNotes:[
        {id:'note-1',fix:heldFix,kind:'question',blocking:true,path:'src/長いファイル名と行の変更を確認するための計算処理.ts',startLine:2,endLine:4,text:'Could this divisor be zero? The saved function always divides by zero.',outdated:false},
        {id:'note-2',kind:'suggestion',blocking:false,path:'math.rs',startLine:1,endLine:1,text:'Consider naming the input.',outdated:true}
      ]}));
      ipcMain.removeHandler('intercat:start-held');ipcMain.handle('intercat:start-held',(_event,request)=>{if(request.utteranceId!=='fix-input'||request.revision!==2)throw new Error('Unexpected fix identity');heldFix.state='planned';word.state='placed';word.reviewFix.state='planned';state.sequence++;return {};});
      ipcMain.removeHandler('intercat:review-retry');ipcMain.handle('intercat:review-retry',(_event,request)=>{
        const fixture=(globalThis as typeof globalThis & {reviewFixture:{requests:unknown[]}}).reviewFixture;
        fixture.requests.push(request);if(fixture.requests.length===1)throw new Error('Dummy lost acknowledgement');
        review.state='queued';review.revision=2;state.sequence++;return review;
      });
    },zoom);
    const page=await app.firstWindow();await page.reload();
    await expect(page.getByRole('region',{name:'Timeline',exact:true})).toHaveAttribute('data-ready','true');
    await expect(page.getByRole('button',{name:'Cut fix: math.rs from A',exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Open fix: math.rs · held · Start',exact:true}).click();
    const notes=page.getByRole('region',{name:'Reviewer notes',exact:true});
    await expect(notes).toContainText('question · required');
    await expect(notes).toContainText('suggestion · optional');
    await expect(notes).toContainText('may be outdated');
    await notes.getByRole('button',{name:'Start fix',exact:true}).click();
    await expect(notes.getByRole('button',{name:'Start fix',exact:true})).toHaveCount(0);
    const retry=notes.getByRole('button',{name:'Retry review',exact:true});
    await retry.click();await expect(notes.getByRole('alert')).toContainText('Dummy lost acknowledgement');
    await retry.click();await expect(notes).toContainText('Waiting to review');
    const requests=await app.evaluate(()=> (globalThis as typeof globalThis & {reviewFixture:{requests:unknown[]}}).reviewFixture.requests);
    expect(requests).toHaveLength(2);expect(requests[0]).toEqual(requests[1]);
    await notes.getByText('Consider naming the input.', { exact: true }).scrollIntoViewIfNeeded();
    await expect(notes.getByText('Consider naming the input.', { exact: true })).toBeInViewport();
    const bounds=await notes.evaluate(e=>({width:e.clientWidth,content:e.scrollWidth}));expect(bounds.content).toBeLessThanOrEqual(bounds.width);
    const pixels=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('review-notes.png'),Buffer.from(pixels,'base64'));
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});


for (const zoom of [1, 2]) test(`Review conflict preserves one choice and attempt history at ${zoom * 100}%`, async () => {
  const directory = await mkdtemp('/private/tmp/ic-review-conflict-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  try {
    await mockDrafts(app);
    await app.evaluate(({ ipcMain, BrowserWindow }, zoom) => {
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);
      const fix={id:'fix-a',revision:3,noteId:'note-a',inputId:'fix-input',trackId:'A',state:'blocked',path:'src/math.ts',clipId:'clip-a',text:'The saved implementation still divides by zero.',reason:'Reviewer still flags src/math.ts after 2 fixes',fileAttempts:2,trackAttempts:2,attempts:[{inputId:'first-input',state:'unresolved',reason:'The first correction did not resolve this finding.'},{inputId:'fix-input',state:'blocked',reason:'The second correction did not resolve this finding.'}]};
      const fixture={state:{connected:true,sequence:1,workers:[],tracks:[{id:'A',label:'A',title:'Dummy math',status:'stopped'}],utterances:[],reviewFixes:[fix]},requests:[] as unknown[]};
      Object.assign(globalThis,{conflictFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>fixture.state);
      ipcMain.removeHandler('intercat:review-fix');ipcMain.handle('intercat:review-fix',(_event,request)=>{fixture.requests.push(request);if(fixture.requests.length===1)throw new Error('Dummy lost acknowledgement');fix.state='planned';fix.revision++;fixture.state.sequence++;return fix;});
    },zoom);
    const page=await app.firstWindow();await page.reload();
    const card=page.getByRole('group',{name:'Review needs you · A',exact:true});
    await expect(card).toContainText('after 2 fixes');
    const box=await card.evaluate(e=>({width:e.clientWidth,content:e.scrollWidth}));expect(box.content).toBeLessThanOrEqual(box.width);
    const flow=await card.evaluate(e=>{const heading=e.querySelector('h3')!.getBoundingClientRect(),location=e.querySelector('.review-location')!.getBoundingClientRect();return {headingEnd:heading.bottom,locationStart:location.top};});expect(flow.locationStart).toBeGreaterThanOrEqual(flow.headingEnd);
    await card.getByRole('button',{name:'Fix once more',exact:true}).hover();
    await card.getByRole('heading',{name:'Review needs you · A',exact:true}).scrollIntoViewIfNeeded();
    await expect(card.getByRole('heading',{name:'Review needs you · A',exact:true})).toBeInViewport();
    await writeFile(test.info().outputPath('review-conflict-heading.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await card.getByText('Review and fix history',{exact:true}).click();
    await expect(card).toContainText('The first correction did not resolve this finding.');
    const again=card.getByRole('button',{name:'Fix once more',exact:true});
    await again.scrollIntoViewIfNeeded();await expect(again).toBeInViewport();
    await writeFile(test.info().outputPath('review-conflict.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await again.click();await expect(card.getByRole('alert')).toContainText('Dummy lost acknowledgement');
    await expect(card.getByRole('button',{name:'Keep it',exact:true})).toBeDisabled();
    await again.click();await expect(card).toHaveCount(0);
    const requests=await app.evaluate(()=>(globalThis as typeof globalThis & {conflictFixture:{requests:unknown[]}}).conflictFixture.requests);
    expect(requests).toHaveLength(2);expect(requests[0]).toEqual(requests[1]);
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
