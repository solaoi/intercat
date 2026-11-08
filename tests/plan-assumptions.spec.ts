import { _electron as electron, expect, test, type ElectronApplication, type Page } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import type { DaemonStatus, PlanReplyRequest } from '../src/bridge';
import type { PlanCommand } from '../src/plans/types';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';

let app: ElectronApplication, page: Page, directory: string;
const card = () => page.locator('[data-attention-card^="planning-assumption-"]');
test.beforeEach(async () => {
  directory = await mkdtemp('/private/tmp/intercat-planning-assumptions-ui-');
  app = await electron.launch({ args: ['.', '--preview', `--user-data-dir=${directory}`] });
  await mockDrafts(app); page = await app.firstWindow();
  await page.clock.install({ time: Date.now() }); await page.clock.pauseAt(Date.now() + 1000);
});
test.afterEach(async () => { await closeWindow(app); await rm(directory, { recursive: true, force: true }); });
async function mount(zoom=1, lose=false) {
  await app.evaluate(({ipcMain,BrowserWindow}, {zoom,lose}) => {
    const state: DaemonStatus = { connected: true, sequence: 1, workerConfigured: true, tracks: [{id:'B',label:'B',title:'Dummy work',status:'ready'}],
      workers: [], utterances: [], session: {id:'dummy-session',name:'Dummy plan',slug:'dummy',createdAt:1},
      planAssumptions: [{id:'assumption',assumptionId:'assumption',planId:'plan',planRevision:2,label:'ダミーの報告を作る',text:'Which format should the dummy report use?',detail:'Assumed: JSON'}] };
    const fixture={state,keep:[] as PlanCommand[],reply:[] as PlanReplyRequest[],receipts:new Map<string,unknown>(),lose};
    Object.assign(globalThis,{planningAssumptions:fixture});
    ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status',()=>state);
    ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
    ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
      fixture.keep.push(request);
      if(fixture.receipts.has(request.opId)) { state.planAssumptions=[]; state.sequence!++; return fixture.receipts.get(request.opId); }
      fixture.receipts.set(request.opId,{});
      if(fixture.lose){fixture.lose=false;throw new Error('Dummy lost acknowledgement');}
      state.planAssumptions=[]; state.sequence!++; return {};
    });
    ipcMain.removeHandler('intercat:plan-reply'); ipcMain.handle('intercat:plan-reply',(_event,request:PlanReplyRequest)=>{
      fixture.reply.push(request); state.planAssumptions=[]; state.sequence!++;
      return {inputId:request.opId,planId:request.planId,state:'landed'};
    });
    BrowserWindow.getAllWindows()[0].setSize(1040,900); BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
  },{zoom,lose});
  await page.reload(); await page.clock.runFor(1200); await page.mouse.move(5,5); await page.clock.runFor(1600);
}
for(const zoom of [1,2]) test(`planning defaults offer readable Keep and Change at ${zoom*100}%`,async()=>{
  await mount(zoom);
  await expect(card()).toContainText('Plan assumed · JSON');
  await card().getByText('Details',{exact:true}).click();
  await expect(card()).toContainText('ダミーの報告を作る');
  for(const button of await card().getByRole('button').all()) { await button.scrollIntoViewIfNeeded();await expect(button).toBeInViewport({ratio:1}); }
  const bounds=await card().evaluate(element=>({width:element.clientWidth,scroll:element.scrollWidth})); expect(bounds.scroll).toBeLessThanOrEqual(bounds.width+1);
  await writeFile(test.info().outputPath(`plan-assumption-${zoom*100}.png`),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
  await card().getByRole('button',{name:'Keep',exact:true}).click();
  await expect.poll(()=>app.evaluate(()=>(globalThis as any).planningAssumptions.keep)).toEqual([expect.objectContaining({planId:'plan',revision:2,action:{kind:'keep-assumption',assumptionId:'assumption'}})]);
});
test('Keep retries exactly the saved choice after a lost acknowledgement',async()=>{
  await mount(1,true); await card().getByRole('button',{name:'Keep',exact:true}).click();
  await expect(card().getByRole('alert')).toContainText('Dummy lost acknowledgement');
  await expect(card().getByRole('button',{name:'Change',exact:true})).toBeDisabled();
  await card().getByRole('button',{name:'Retry saving answer',exact:true}).click();
  const requests=await app.evaluate(()=>(globalThis as any).planningAssumptions.keep); expect(requests).toHaveLength(2); expect(requests[0]).toEqual(requests[1]);
});
test('Change opens a durable plan reply and restores the ordinary draft after sending',async()=>{
  await mount();const draft=page.getByRole('textbox',{name:'Request draft',exact:true});
  await draft.fill('Keep this B draft.');await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
  await card().getByRole('button',{name:'Change',exact:true}).click();await page.clock.runFor(32);
  await expect(draft).toHaveValue('');await expect(draft).toBeFocused();
  await draft.fill('Use CSV for the dummy report.');await page.reload();await page.clock.runFor(1200);
  await expect(draft).toHaveValue('Use CSV for the dummy report.');
  await page.getByRole('button',{name:'Send reply',exact:false}).click();
  await expect(draft).toHaveValue('Keep this B draft.');await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
  const requests=await app.evaluate(()=>(globalThis as any).planningAssumptions.reply);
  expect(requests).toEqual([expect.objectContaining({planId:'plan',questionId:'assumption',revision:2,question:'Which format should the dummy report use?\nAssumed: JSON',text:'Use CSV for the dummy report.'})]);
});
