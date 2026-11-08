import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { DaemonStatus, PlanDependencyLocation } from '../src/bridge';
import type { Plan, PlanCommand } from '../src/plans/types';

const planId='00000000-0000-5000-8000-000000000010';
const parentId='00000000-0000-5000-8000-000000000011';
const childId='00000000-0000-5000-8000-000000000012';
const parentPlanId='00000000-0000-5000-8000-000000000013';
for(const zoom of [1,2]) for(const cut of [false,true]) for(const startWithout of [false,true]) for(const implicit of startWithout && !cut ? [false,true] : [false]) test(`${implicit?'earlier plan order':'dependency'} ${startWithout?'start without':cut?'restore':'retry'} ${cut?'cut':'failed'} preserves the draft and exact operation at ${zoom*100}%`,async({},info)=>{
  const directory=await mkdtemp('/private/tmp/ic-plan-dependency-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  try{
    await app.evaluate(({ipcMain,BrowserWindow},{planId,parentId,childId,parentPlanId,cut,zoom,startWithout,implicit})=>{
      const plan:Plan={id:planId,rev:3,state:startWithout?'held':'running',sourceId:'source',sourceRevision:1,startMode:'manual',executeAllowed:true,lockedTrack:null,
        generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Dummy dependency actions',assumptions: [], notices:[],seenRevision:null,rewrite:null,
        items:[{planItemId:parentId,trackRef:'A',folderId:'dummy',order:0,title:'Dummy parent',instruction:'Create a dummy result.',constraints:[],doneWhen:'Dummy result exists.',after:[],state:cut?'cut':'failed',skipOrder:[],startRequested:false,utteranceId:cut?null:'parent-input',detail:'Dummy result needs work.'},
          {planItemId:childId,trackRef:implicit?'A':'B',folderId:'dummy',order:1,title:'Dummy child',instruction:'Use the dummy result.',constraints:[],doneWhen:'Dummy child exists.',after:implicit?[]:[{id:parentId,inherit:false}],state:'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:'held'}]};
      const parentPlan:Plan={...structuredClone(plan),id:parentPlanId,state:'done',items:[plan.items[0]]};
      plan.items=[plan.items[1]];
      const parent:PlanDependencyLocation={id:parentId,planId:parentPlanId,itemId:parentId,retryable:!cut,trackId:'A',title:'Dummy parent',state:cut?'cut':'failed',unclear:!cut,clipId:cut?null:'saved-parent-clip',atMs:cut?null:1000};
      const others:Record<string,PlanDependencyLocation>={};
      if(!cut&&zoom===2&&!startWithout){
        const id='00000000-0000-5000-8000-000000000014', title='別のダミー依存元：長い日本語の結果と複数の操作があっても、メニューは窓の中に収まる';
        parentPlan.items.push({...parentPlan.items[0],planItemId:id,trackRef:'C',title});
        plan.items[0].after.push({id,inherit:false});
        others[id]={...parent,id,itemId:id,trackId:'C',title,clipId:null};
      }
      const saved={id:'saved-parent-clip',trackId:'A',repairRunId:null,label:'Saved dummy result',kind:'think',category:'step',start:100,end:1000,ordinal:1,running:false,turnId:'saved-turn',generationEnd:1000};
      const state:DaemonStatus={connected:true,workerConfigured:true,sequence:1,plans:implicit?[parentPlan,plan]:[plan,parentPlan],planDependencies:{[parentId]:parent,...others},workers:[],utterances:[],tracks:[{id:'A',label:'A',title:'Dummy parent worker',status:'done'},{id:'B',label:'B',title:'Dummy child worker',status:implicit?'done':'planned'}]};
      const fixture={state,calls:[] as PlanCommand[],windows:[] as unknown[],receipts:new Set<string>(),lose:true};
      Object.assign(globalThis,{dependencyFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',(_event,request)=>{fixture.windows.push(request);return {revision:1,reset:true,clips:request.start===999&&request.end===1001?[saved]:[],marks:[],removed:[],removedMarks:[]};});
      ipcMain.removeHandler('intercat:clip-detail');ipcMain.handle('intercat:clip-detail',()=>({clip:saved,items:[],model:null}));
      ipcMain.removeHandler('intercat:plan-command');ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
        fixture.calls.push(request);
        const target=startWithout?plan:parentPlan;
        if(fixture.receipts.has(request.opId))return structuredClone(target);
        if(request.planId!==target.id||request.revision!==target.rev)throw new Error('Wrong plan or revision');
        if(startWithout){
          if(request.action.kind!=='start-without'||request.action.itemId!==childId||request.action.dependencyId!==parentId)throw new Error('Wrong start-without operation');
          plan.items[0].after=[];plan.items[0].skipOrder=implicit?[parentId]:[];plan.items[0].startRequested=true;plan.items[0].detail=null;
        }else{
        if(!('itemId' in request.action)||request.action.itemId!==parentId||request.action.kind!==(cut?'restore':'retry-item'))throw new Error('Wrong dependency operation');
        if(cut){parentPlan.items[0].state='planned';parent.state='planned';parentPlan.state='held';}
        else parent.retryable=false;
        }
        fixture.receipts.add(request.opId);target.rev++;state.sequence!++;
        if(fixture.lose){fixture.lose=false;throw new Error('Dummy dependency acknowledgement lost');}
        return structuredClone(target);
      });
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);
    },{planId,parentId,childId,parentPlanId,cut,zoom,startWithout,implicit});
    const page=await app.firstWindow(),draft=page.getByRole('textbox',{name:'Request draft',exact:true});
    await draft.fill('Keep this ordinary dummy draft.');
    await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
    const tile=page.locator(`[data-plan-item-id="${childId}"] .planned-item-select`);
    await page.getByRole('scrollbar',{name:'All tracks',exact:true}).press('End');
    await tile.click();
    const menu=page.getByRole('menu',{name:'Dependency actions for Dummy child',exact:true});
    await expect(menu).toBeVisible();
    await expect(menu).toContainText(cut?'was cut':'result unclear');
    await app.evaluate((_electron,parentPlanId)=>{const state=(globalThis as any).dependencyFixture.state;state.plans.find((plan:Plan)=>plan.id===parentPlanId).rev++;state.sequence++;},parentPlanId);
    await expect(menu).toContainText('The dependency changed');
    const actionName=startWithout?'Start anyway without Dummy parent':cut?'Restore Dummy parent':'Retry Dummy parent’s item';
    await expect(menu.getByRole('menuitem',{name:actionName,exact:true})).toBeDisabled();
    await page.keyboard.press('Escape');await tile.click();
    await expect(menu.getByRole('menuitem',{name:actionName,exact:true})).toBeEnabled();
    const box=await menu.boundingBox(),size=await page.evaluate(()=>({width:innerWidth,height:innerHeight}));
    expect(box!.x).toBeGreaterThanOrEqual(0);expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.x+box!.width).toBeLessThanOrEqual(size.width);expect(box!.y+box!.height).toBeLessThanOrEqual(size.height);
    await writeFile(info.outputPath('dependency-actions.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await page.keyboard.press('End');
    await expect(menu.getByRole('menuitem',{name:'Revise instructions…',exact:true})).toBeFocused();
    await expect(menu.getByRole('menuitem',{name:'Revise instructions…',exact:true})).toBeInViewport();
    await page.keyboard.press('Home');
    if(!cut){
      await menu.getByRole('menuitem',{name:'Open Dummy parent',exact:true}).click();
      await expect(page.locator('.clip-inspector')).toContainText('Saved dummy result');
      expect(await app.evaluate(()=>(globalThis as any).dependencyFixture.windows.some((request:any)=>request.start===999&&request.end===1001))).toBe(true);
      await expect(draft).toHaveValue('Keep this ordinary dummy draft.');
      await page.getByRole('scrollbar',{name:'All tracks',exact:true}).press('End');
      await tile.focus();await tile.press('Control+Enter');
      await expect(menu).toBeVisible();
    }
    await menu.getByRole('menuitem',{name:actionName,exact:true}).click();
    await page.getByRole('button',{name:'Retry plan action',exact:true}).click();
    await expect(page.getByRole('button',{name:'Retry plan action',exact:true})).toHaveCount(0);
    const calls=await app.evaluate(()=>(globalThis as any).dependencyFixture.calls as PlanCommand[]);
    expect(calls).toHaveLength(2);expect(calls[0]).toEqual(calls[1]);
    expect(calls[0].planId).toBe(startWithout?planId:parentPlanId);
    expect(calls[0].action).toEqual(startWithout?{kind:'start-without',itemId:childId,dependencyId:parentId}:{kind:cut?'restore':'retry-item',itemId:parentId});
    if(startWithout){
      await expect(tile).toHaveAccessibleName('Dummy child · queued');
      expect(await app.evaluate((_electron,planId)=>(globalThis as any).dependencyFixture.state.plans.find((plan:Plan)=>plan.id===planId).state,planId)).toBe('held');
    }
    await expect(draft).toHaveValue('Keep this ordinary dummy draft.');
    await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
