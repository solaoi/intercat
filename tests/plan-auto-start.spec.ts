import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import { resolve } from 'node:path';
import type { Plan, PlanCommand, PlanObservation, PlansStartChange } from '../src/plans/types';

for (const zoom of [1, 2]) test(`Automatic plans wait through review and Hold at ${zoom * 100}%`, async ({}, info) => {
  const directory = await mkdtemp('/private/tmp/ic-plan-auto-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + directory] });
  await mockDrafts(app);
  try {
    await app.evaluate(async ({ ipcMain, BrowserWindow, app }, { zoom, native, bridge }) => {
      const { observePlans, planWindowFocused } = process.getBuiltinModule('module').createRequire(bridge)(bridge);
      const plan: Plan = { id:'00000000-0000-5000-8000-000000000001', rev:2, state:'draft', sourceId:'source', sourceRevision:1,
        startMode:'after-delay', executeAllowed:true, lockedTrack:null, generation:{id:'generation',state:'ready',attempt:0,detail:null}, basedOn:[], attachments:[], summary:'Make a dummy badge.', assumptions: [], notices:[], seenRevision:null, rewrite:null,
        items:[{planItemId:'item-1',trackRef:'new:1',folderId:'dummy',order:0,title:'Dummy badge',instruction:'Make a dummy badge.',constraints:[],doneWhen:'Badge exists.',after:[],state:'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:null}] };
      const state = { plan, nativeFocusStub: !native, observations:[] as PlanObservation[], since:null as number|null, startedAfter:0, calls:[] as PlanCommand[] };
      Object.assign(globalThis,{ autoPlanFixture:state });
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status',()=>({connected:true,sequence:plan.rev,tracks:[],workers:[],utterances:[],plans:[plan]}));
      ipcMain.removeHandler('intercat:timeline-window'); ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:plan-command'); ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
        state.calls.push(request);
        if (request.action.kind==='review') plan.seenRevision=plan.rev;
        if (request.action.kind==='hold') { plan.state='held'; plan.rev++; }
        return structuredClone(plan);
      });
      let sequence = 0;
      ipcMain.removeHandler('intercat:plan-observe'); ipcMain.handle('intercat:plan-observe',(event,request:PlanObservation)=>observePlans(async()=>({request:async(_method:string,request:PlanObservation)=>{
        state.observations.push(request);
        if (!request.idle || plan.state!=='draft' || !request.plans.some(value=>value.planId===plan.id && value.revision===plan.rev)) { state.since=null; return {countdowns:[],started:[]}; }
        state.since ??= performance.now();
        const elapsed=performance.now()-state.since;
        if (elapsed>=5000) { state.startedAfter=elapsed; plan.state='running'; plan.rev++; return {countdowns:[],started:[structuredClone(plan)]}; }
        return {countdowns:[{planId:plan.id,revision:plan.rev,remainingMs:5000-elapsed}],started:[]};
      }}),request,{owner:'dummy-window',sequence:++sequence,focused:()=>!native||planWindowFocused(BrowserWindow.fromWebContents(event.sender),event.sender,'dummy-window','dummy-window')}));
      const win=BrowserWindow.getAllWindows()[0];
      win.setSize(zoom===2 ? 1440 : 1040, zoom===2 ? 1100 : 760); win.webContents.setZoomFactor(zoom); win.show(); app.focus({steal:true}); win.focus();
    },{zoom,native:process.env.INTERCAT_TEST_NATIVE_PLAN_FOCUS==='1',bridge:resolve('electron/plans.mjs')});
    const page=await app.firstWindow();
    await expect(page.locator('.timeline')).toHaveAttribute('data-ready','true');
    await page.evaluate(()=> (document.activeElement as HTMLElement)?.blur()); await page.mouse.move(2,2);
    const strip=page.getByRole('region',{name:'Plans',exact:true});
    await expect(strip).toContainText(/starts in [1-5] s/);
    await expect(page.getByRole('button',{name:/Dummy badge · starts in [1-5] s/})).toBeVisible();
    if (process.env.INTERCAT_TEST_NATIVE_PLAN_FOCUS === '1') {
      await app.evaluate(async ({ BrowserWindow }) => {
        const main = BrowserWindow.getAllWindows()[0];
        const away = new BrowserWindow({ width: 400, height: 240, title: 'Intercat focus probe', webPreferences: { sandbox: true } });
        await away.loadURL('about:blank');
        Object.assign(globalThis, { autoPlanMainWindow: main.id, autoPlanAwayWindow: away.id });
        away.show(); away.focus();
      });
      await expect.poll(() => app.evaluate(({ BrowserWindow }) => BrowserWindow.fromId((globalThis as any).autoPlanMainWindow)!.isFocused())).toBe(false);
      await page.waitForTimeout(5500);
      expect(await app.evaluate(() => (globalThis as any).autoPlanFixture.plan.state)).toBe('draft');
      expect(await app.evaluate(() => (globalThis as any).autoPlanFixture.observations.at(-1).idle)).toBe(false);
      await app.evaluate(({ BrowserWindow, app }) => {
        BrowserWindow.fromId((globalThis as any).autoPlanAwayWindow)!.close();
        const main = BrowserWindow.fromId((globalThis as any).autoPlanMainWindow)!;
        main.show(); app.focus({ steal: true }); main.focus();
      });
      await expect.poll(() => app.evaluate(({ BrowserWindow }) => BrowserWindow.fromId((globalThis as any).autoPlanMainWindow)!.isFocused())).toBe(true);
      await page.evaluate(() => (document.activeElement as HTMLElement)?.blur());
      await expect(strip).toContainText(/starts in [1-5] s/);
    }
    await page.getByRole('button',{name:'Overview',exact:true}).click();
    await expect(strip).not.toContainText(/starts in [1-5] s/);
    await expect.poll(()=>app.evaluate(()=> (globalThis as any).autoPlanFixture.observations.at(-1).plans)).toEqual([]);
    await page.getByRole('button',{name:'Detail',exact:true}).click();
    await expect(strip).toContainText(/starts in [1-5] s/);
    await page.getByRole('textbox',{name:'Request draft'}).focus();
    await expect(strip).not.toContainText(/starts in [1-5] s/);
    await expect.poll(()=>app.evaluate(()=> (globalThis as any).autoPlanFixture.observations.at(-1).idle)).toBe(false);
    await page.evaluate(()=> (document.activeElement as HTMLElement)?.blur());
    await expect(strip).toContainText(/starts in [1-5] s/);
    await strip.getByRole('button',{name:'Review plan',exact:true}).click();
    const review=page.getByRole('dialog',{name:'Review plan'});
    await expect(review).toBeVisible();
    await page.waitForTimeout(5500);
    expect(await app.evaluate(()=> (globalThis as any).autoPlanFixture.plan.state)).toBe('draft');
    expect(await app.evaluate(()=> (globalThis as any).autoPlanFixture.observations.at(-1).idle)).toBe(false);
    await review.getByRole('button',{name:'Close plan review'}).click();
    await page.evaluate(()=> (document.activeElement as HTMLElement)?.blur()); await page.mouse.move(2,2);
    await expect(strip).toContainText(/starts in [1-5] s/);
    await strip.getByRole('button',{name:'Hold',exact:true}).click();
    await expect(strip.locator('.plan-strip-copy')).toContainText('held');
    await expect(page.getByRole('button',{name:'Dummy badge · held',exact:true})).toBeVisible();
    await expect(strip.getByRole('button',{name:'Start plan · 1 track',exact:true})).toBeEnabled();
    await expect(strip.getByRole('button',{name:'Hold',exact:true})).toHaveCount(0);
    await writeFile(info.outputPath('held-plan.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await app.evaluate(()=> { const state=(globalThis as any).autoPlanFixture; state.plan.state='draft'; state.plan.rev++; state.since=null; });
    await page.evaluate(()=> (document.activeElement as HTMLElement)?.blur()); await page.mouse.move(2,2);
    await expect(strip).toContainText(/starts in [1-5] s/);
    await expect(strip).toContainText('running',{timeout:9000});
    expect(await app.evaluate(()=> (globalThis as any).autoPlanFixture.startedAfter)).toBeGreaterThanOrEqual(5000);
  } finally {
    {
      const page=await app.firstWindow();
      await writeFile(info.outputPath('observations.json'),JSON.stringify({
        main:await app.evaluate(({BrowserWindow})=>({focused:BrowserWindow.getAllWindows()[0].isFocused(),fixture:(globalThis as any).autoPlanFixture})),
        page:await page.evaluate(()=>({focused:document.hasFocus(),active:document.activeElement?.tagName,visibility:document.visibilityState,modal:document.querySelector('dialog[open],[popover]:popover-open')?.outerHTML,tiles:[...document.querySelectorAll('[data-auto-plan]')].map(tile=>({rect:tile.getBoundingClientRect().toJSON(),visible:tile.checkVisibility(),stage:tile.closest('.timeline')?.getBoundingClientRect().toJSON()}))}))
      },null,2));
    }
    await closeWindow(app); await rm(directory,{recursive:true,force:true});
  }
});

for (const zoom of [1, 2]) test(`Plans start setting keeps its operation after a lost response at ${zoom * 100}%`, async ({}, info) => {
  const directory=await mkdtemp('/private/tmp/ic-plans-start-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  try {
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      ipcMain.removeHandler('intercat:status'); ipcMain.handle('intercat:status',()=>({connected:true,tracks:[],workers:[],utterances:[]}));
      let setting={revision:0,mode:'manual'};
      const calls:PlansStartChange[]=[];
      Object.assign(globalThis,{plansStartCalls:calls});
      ipcMain.removeHandler('intercat:plans-start-get'); ipcMain.handle('intercat:plans-start-get',()=>setting);
      ipcMain.removeHandler('intercat:plans-start-change'); ipcMain.handle('intercat:plans-start-change',(_event,request:PlansStartChange)=>{
        calls.push(request);
        if (calls.length===1) { setting={revision:1,mode:request.mode}; throw new Error('Dummy lost save response.'); }
        return setting;
      });
      const win=BrowserWindow.getAllWindows()[0]; win.setSize(1040,760); win.webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();
    await page.getByRole('button',{name:'Settings',exact:true}).click();
    const setting=page.getByRole('region',{name:'Plans start',exact:true});
    const choice=setting.getByRole('combobox');
    await expect(choice).toHaveValue('manual');
    await choice.selectOption('after-delay');
    await setting.getByRole('button',{name:'Apply Plans start',exact:true}).click();
    await expect(setting.getByRole('alert')).toContainText('Dummy lost save response.');
    await expect(choice).toBeDisabled();
    await setting.getByRole('button',{name:'Retry Plans start',exact:true}).click();
    await expect(choice).toBeEnabled(); await expect(choice).toHaveValue('after-delay');
    const calls=await app.evaluate(()=>(globalThis as any).plansStartCalls);
    expect(calls).toHaveLength(2); expect(calls[0]).toEqual(calls[1]);
    await setting.scrollIntoViewIfNeeded();
    await writeFile(info.outputPath('plans-start-setting.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await page.getByRole('button',{name:'Close settings',exact:true}).click();
  } finally { await closeWindow(app); await rm(directory,{recursive:true,force:true}); }
});
