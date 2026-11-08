import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { DaemonStatus } from '../src/bridge';
import type { Plan, PlanCommand } from '../src/plans/types';

const planId='00000000-0000-5000-8000-000000000031', itemId='00000000-0000-5000-8000-000000000032';
const clipId='00000000-0000-5000-8000-000000000033', newClipId='00000000-0000-5000-8000-000000000034';
for(const zoom of [1,2])test(`External dependency retry captures its clip and preserves the draft at ${zoom*100}%`,async({},info)=>{
  const directory=await mkdtemp('/private/tmp/ic-external-retry-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  try{
    await app.evaluate(({ipcMain,BrowserWindow},{planId,itemId,clipId,zoom})=>{
      const plan:Plan={id:planId,rev:3,state:'held',sourceId:'source',sourceRevision:1,startMode:'manual',executeAllowed:true,lockedTrack:null,
        generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Use the saved dummy result.',assumptions: [], notices:[],seenRevision:null,rewrite:null,
        items:[{planItemId:itemId,trackRef:'B',folderId:'dummy',order:0,title:'Dummy child',instruction:'Use the saved dummy result.',constraints:[],doneWhen:'The dummy child exists.',after:[{id:clipId,inherit:false}],state:'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:'held'}]};
      const state:DaemonStatus={connected:true,workerConfigured:true,sequence:1,plans:[plan],workers:[],utterances:[],tracks:[{id:'A',label:'A',title:'Original dummy task',status:'done'},{id:'B',label:'B',title:'Dummy child',status:'planned'}],
        planDependencies:{[clipId]:{id:clipId,planId:null,itemId:null,retryable:true,trackId:'A',title:'Original dummy task',state:'failed',unclear:true,clipId,atMs:1000}}};
      const fixture={state,calls:[] as PlanCommand[],receipts:new Set<string>()};Object.assign(globalThis,{externalRetryFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:plan-command');ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
        fixture.calls.push(request);if(fixture.receipts.has(request.opId))return structuredClone(plan);
        const parent=state.planDependencies![clipId];
        if(request.planId!==plan.id||request.revision!==plan.rev||request.action.kind!=='retry-dependency'||request.action.itemId!==itemId||request.action.dependencyId!==clipId||request.action.clipId!==parent.clipId)throw new Error('Wrong captured dependency retry');
        fixture.receipts.add(request.opId);plan.rev++;state.sequence!++;parent.state='planned';parent.retryable=false;
        state.utterances!.push({id:'retry-input',revision:1,text:'Retry the selected saved task.',target:{mode:'lock',trackId:'A'},state:'placed',createdAt:0,attachments:[],reason:null,
          dependencyRetry:{clipId,title:parent.title},dependencyRetryRunnable:true,decision:{kind:'extend',trackId:'A',boundary:'after-turn',command:null,confidence:1,inferenceMs:0,timedOut:false}});
        throw new Error('Dummy acknowledgement lost');
      });
      const win=BrowserWindow.getAllWindows()[0];win.setSize(1040,760);win.webContents.setZoomFactor(zoom);
    },{planId,itemId,clipId,zoom});
    const page=await app.firstWindow(),draft=page.getByRole('textbox',{name:'Request draft',exact:true});await draft.fill('Keep the independent draft.');
    await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
    const tile=page.locator(`[data-plan-item-id="${itemId}"] .planned-item-select`);await tile.click();
    const menu=page.getByRole('menu',{name:'Dependency actions for Dummy child',exact:true});
    const retry=menu.getByRole('menuitem',{name:'Retry Original dummy task’s item',exact:true});await expect(retry).toBeEnabled();
    await app.evaluate((_electron,{clipId,newClipId})=>{const state=(globalThis as any).externalRetryFixture.state;state.planDependencies[clipId].clipId=newClipId;state.sequence++;},{clipId,newClipId});
    await expect(retry).toBeDisabled();await page.keyboard.press('Escape');await tile.click();await expect(retry).toBeEnabled();
    await writeFile(info.outputPath('external-retry.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await retry.click();await page.getByRole('button',{name:'Retry plan action',exact:true}).click();await expect(page.getByRole('button',{name:'Retry plan action',exact:true})).toHaveCount(0);
    await expect(page.getByText('Retry Original dummy task',{exact:true})).toBeVisible();
    await expect(draft).toHaveValue('Keep the independent draft.');await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
    const result=await app.evaluate(()=>(globalThis as any).externalRetryFixture);
    expect(result.calls).toHaveLength(2);expect(result.calls[0]).toEqual(result.calls[1]);
    expect(result.calls[0].action).toEqual({kind:'retry-dependency',itemId,dependencyId:clipId,clipId:newClipId});
    expect(result.state.utterances).toHaveLength(1);expect(result.state.plans[0].state).toBe('held');
    await app.evaluate(()=>{const state=(globalThis as any).externalRetryFixture.state;state.utterances[0].state='held';state.utterances[0].dependencyRetryRunnable=false;state.sequence++;});
    await expect(page.getByText('held · retry source changed',{exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'Start request: Retry Original dummy task',exact:true})).toBeDisabled();
    await app.evaluate((_electron,clipId)=>{const state=(globalThis as any).externalRetryFixture.state;Object.assign(state.planDependencies[clipId],{state:'failed',unavailable:true,retryable:false,clipId:null,title:'Unavailable dependency'});state.sequence++;},clipId);
    await expect(tile).toHaveAccessibleName('Dummy child · held · dependency unavailable');
    await page.getByRole('scrollbar',{name:'All tracks',exact:true}).press('End');await tile.click();
    await expect(menu).toContainText('unavailable');
    await expect(menu.getByRole('menuitem',{name:'Open Unavailable dependency',exact:true})).toBeDisabled();
    await expect(menu.getByRole('menuitem',{name:'Start anyway without Unavailable dependency',exact:true})).toBeEnabled();
    await expect(menu.getByRole('menuitem',{name:'Change dependency…',exact:true})).toBeEnabled();
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
