import { _electron as electron, expect, test, type ElectronApplication } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { Plan, PlanCommand } from '../src/plans/types';

const alpha = '00000000-0000-5000-8000-000000000001';
const beta = '00000000-0000-5000-8000-000000000002';

async function mount(app: ElectronApplication, zoom: number) {
  await mockDrafts(app);
  await app.evaluate(({ipcMain, BrowserWindow}, zoom) => {
    const plans: Plan[] = ['Alpha', 'Beta'].map((name,index) => ({
      id:`00000000-0000-5000-8000-00000000000${index+1}`,rev:2,state:index===0?'running':'draft',sourceId:`source-${index}`,sourceRevision:1,
      startMode:'manual',executeAllowed:index===0,lockedTrack:null,generation:{id:`generation-${index}`,state:'ready',attempt:0,detail:null},
      basedOn:[],attachments:[],summary:`Dummy ${name} plan with a long name for the compact layout`,assumptions: [], notices:[],seenRevision:null,rewrite:null,
      items:[0,1].map(item=>({planItemId:`item-${index}-${item}`,trackRef:index===0?'A':'new:1',folderId:'dummy',order:item,title:`Dummy ${name} ${item}`,
        instruction:'Use dummy data only.',constraints:[],doneWhen:'Done.',after:[],state:index===0&&item===0?'running':'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:null})),
    }));
    const state={plans,sequence:1,calls:[] as PlanCommand[],loseStart:true};
    const receipts=new Map<string,string>();
    Object.assign(globalThis,{planPickerFixture:state});
    ipcMain.removeHandler('intercat:status');
    ipcMain.handle('intercat:status',()=>({connected:true,sequence:state.sequence,tracks:[{id:'A',label:'A',title:'Dummy active worker',status:'working'}],workers:[],plans,
      utterances:[0,1,2,3].map(index=>({id:`dummy-question-${index}`,text:`ダミーの質問 ${index+1}: 保存形式と出力先を確認してください。`,state:'asked',revision:1,attachments:[],createdAt:index,target:{mode:'none'},decision:null,reason:'Choose a destination.'}))}));
    ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
    ipcMain.removeHandler('intercat:plan-command');ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
      state.calls.push(request);
      const plan=plans.find(plan=>plan.id===request.planId)!;
      const previous=receipts.get(request.opId);
      if (previous) { if (previous!==JSON.stringify(request)) throw new Error('Operation changed'); return structuredClone(plan); }
      if (request.revision!==plan.rev) throw new Error('The plan changed · review again');
      if (request.action.kind==='review') plan.seenRevision=plan.rev;
      if (request.action.kind==='hold') { plan.state='held';plan.rev++;plan.seenRevision=null; }
      if (request.action.kind==='start') {
        if (plan.seenRevision!==plan.rev) throw new Error('Review this plan first');
        plan.state='running';plan.rev++;plan.items[0].state='running';plan.seenRevision=null;
      }
      state.sequence++;receipts.set(request.opId,JSON.stringify(request));
      if (request.action.kind==='start'&&state.loseStart) { state.loseStart=false;throw new Error('Dummy saved start response was lost.'); }
      return structuredClone(plan);
    });
    const win=BrowserWindow.getAllWindows()[0];win.setSize(1040,760);win.webContents.setZoomFactor(zoom);
  },zoom);
}

for(const zoom of [1,2]) test(`Fixed plan list controls separate plans at ${zoom*100}%`,async({},info)=>{
  const directory=await mkdtemp('/private/tmp/ic-plan-picker-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mount(app,zoom);const page=await app.firstWindow();
    const draft=page.getByRole('textbox',{name:'Request draft'});
    await draft.fill('Preserve this dummy draft.');
    const strip=page.getByRole('region',{name:'Plans',exact:true});
    await expect(strip.locator('.plan-strip-copy')).toContainText('Alpha');
    expect(await strip.evaluate(el=>el.closest('.desk-scroll'))).toBeNull();
    const box=await strip.evaluate(el=>{const box=el.getBoundingClientRect();return {left:box.left,right:box.right,top:box.top,bottom:box.bottom,width:innerWidth,height:innerHeight};});
    expect(box.left).toBeGreaterThanOrEqual(0);expect(box.right).toBeLessThanOrEqual(box.width);expect(box.top).toBeGreaterThanOrEqual(0);expect(box.bottom).toBeLessThanOrEqual(box.height);
    await strip.getByRole('button',{name:'Plans',exact:true}).click();
    const menu=page.getByRole('dialog',{name:'All plans',exact:true});
    const a=menu.locator(`[data-plan-choice="${alpha}"]`),b=menu.locator(`[data-plan-choice="${beta}"]`);
    await expect(a).toContainText('running');await expect(b).toContainText('waiting for your go');
    await expect(a).toContainText('Dummy Alpha plan with a long name');await expect(b).toContainText('Dummy Beta plan with a long name');
    await expect(menu.locator('[data-select-plan]').first()).toBeFocused();
    const bounds=await menu.evaluate(el=>{const r=el.getBoundingClientRect();return {left:r.left,right:r.right,top:r.top,bottom:r.bottom,width:innerWidth,height:innerHeight};});
    expect(bounds.left).toBeGreaterThanOrEqual(0);expect(bounds.right).toBeLessThanOrEqual(bounds.width);expect(bounds.top).toBeGreaterThanOrEqual(0);expect(bounds.bottom).toBeLessThanOrEqual(bounds.height);
    await writeFile(info.outputPath('plan-list.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await page.keyboard.press('Tab');await expect(a.getByRole('button',{name:'Review plan',exact:true})).toBeFocused();
    await page.keyboard.press('Tab');await expect(a.getByRole('button',{name:'Hold the rest',exact:true})).toBeFocused();await page.keyboard.press('Enter');
    await expect(menu).toHaveCount(0);await expect(strip.locator('.plan-strip-copy')).toContainText('held');
    await strip.getByRole('button',{name:'Plans',exact:true}).click();
    await b.locator('[data-select-plan]').focus();await page.keyboard.press('Tab');await page.keyboard.press('Tab');
    await expect(b.getByRole('button',{name:'Start plan · 1 track',exact:true})).toBeFocused();await page.keyboard.press('Enter');
    await expect(page.getByRole('alert').filter({hasText:'Dummy saved start response was lost.'})).toBeVisible();
    if (zoom===2) {
      await strip.getByRole('button',{name:'Plans',exact:true}).click();
      await a.getByRole('button',{name:'Review plan',exact:true}).click();
      await expect(page.getByRole('dialog',{name:'Review plan',exact:true})).toContainText('Dummy Alpha plan');
    }
    await page.getByRole('button',{name:'Retry plan action',exact:true}).click();
    await expect(page.getByRole('button',{name:'Retry plan action',exact:true})).toHaveCount(0);
    if (zoom===2) await page.getByRole('button',{name:'Back to plans',exact:true}).click();
    const state=await app.evaluate(()=>(globalThis as typeof globalThis&{planPickerFixture:{plans:Plan[];calls:PlanCommand[]}}).planPickerFixture);
    expect(state.plans[0].state).toBe('held');expect(state.plans[0].items.map(item=>item.state)).toEqual(['running','planned']);
    expect(state.plans[1].state).toBe('running');
    const starts=state.calls.filter(call=>call.action.kind==='start');expect(starts).toHaveLength(2);expect(starts[0]).toEqual(starts[1]);expect(starts[0].planId).toBe(beta);
    await expect(draft).toHaveValue('Preserve this dummy draft.');
    await strip.getByRole('button',{name:'Plans',exact:true}).click();
    await a.locator('[data-select-plan]').click();await expect(strip.locator('.plan-strip-copy')).toContainText('Alpha');
    await expect(strip.getByRole('button',{name:'Plans',exact:true})).toBeFocused();
    const scroll=page.locator('.desk-scroll');await scroll.evaluate(el=>{el.scrollTop=el.scrollHeight;});
    const after=await strip.boundingBox();expect(after!.y).toBeCloseTo(box.top,0);await expect(strip.getByRole('button',{name:'Start plan · 1 track',exact:true})).toBeInViewport();
    await expect(page.locator('.needs-you')).not.toContainText('Dummy Alpha plan');
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});

test('A visible plan list keeps its versions until explicitly refreshed',async()=>{
  const directory=await mkdtemp('/private/tmp/ic-plan-picker-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mount(app,1);const page=await app.firstWindow();
    await page.getByRole('button',{name:'Plans',exact:true}).click();const menu=page.getByRole('dialog',{name:'All plans',exact:true});
    await app.evaluate(()=>{const state=(globalThis as typeof globalThis&{planPickerFixture:{plans:Plan[];sequence:number}}).planPickerFixture;state.plans[1].summary='Dummy Beta revised';state.plans[1].rev++;state.sequence++;});
    const b=menu.locator(`[data-plan-choice="${beta}"]`);
    await expect(b).toContainText('changed elsewhere');await expect(b).toContainText('Dummy Beta plan with a long name');
    await expect(b.getByRole('button',{name:'Start plan · 1 track',exact:true})).toBeDisabled();
    await menu.getByRole('button',{name:'Refresh plans',exact:true}).click();
    await expect(b).toContainText('Dummy Beta revised');await expect(b.getByRole('button',{name:'Start plan · 1 track',exact:true})).toBeEnabled();
    await b.getByRole('button',{name:'Review plan',exact:true}).click();
    const review=page.getByRole('dialog',{name:'Review plan',exact:true});await expect(review).toContainText('Dummy Beta revised');
    await expect(review.getByRole('button',{name:/Start plan|Hold the rest/})).toHaveCount(0);
    await review.getByRole('button',{name:'Back to plans',exact:true}).click();
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
