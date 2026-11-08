import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { DaemonStatus, Utterance } from '../src/bridge';
import type { Plan, PlanCommand } from '../src/plans/types';
import type { WorkSubmission } from '../src/desk/drafts';

const planId='00000000-0000-5000-8000-000000000001';
const itemId='00000000-0000-5000-8000-000000000002';

for(const zoom of [1,2]) test(`item instructions keep their scope, draft and retry at ${zoom*100}%`,async({},info)=>{
  const directory=await mkdtemp('/private/tmp/ic-plan-context-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  try{
    await app.evaluate(({ipcMain,BrowserWindow},{planId,itemId,zoom})=>{
      const plan:Plan={id:planId,rev:3,state:'running',sourceId:'source',sourceRevision:1,startMode:'manual',executeAllowed:true,lockedTrack:null,
        generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Dummy item instructions',assumptions: [], notices:[],seenRevision:null,rewrite:null,
        items:[{planItemId:itemId,trackRef:'A',folderId:'folder',order:0,title:'Dummy A result',instruction:'Create a dummy result.',constraints:[],doneWhen:'The dummy check passes.',after:[],state:'failed',skipOrder:[],startRequested:false,utteranceId:'first-input',detail:'One dummy check remains.'}]};
      const state:DaemonStatus={connected:true,workerConfigured:true,sequence:1,plans:[plan],workers:[],utterances:[],
        tracks:[{id:'A',label:'A',title:'Dummy result',status:'done'},{id:'B',label:'B',title:'Original draft destination',status:'ready'}]};
      const fixture={state,calls:[] as WorkSubmission[],saved:new Map<string,Utterance>(),lose:true};
      Object.assign(globalThis,{planContextFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:plan-command');ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
        if(request.revision!==plan.rev)throw new Error('The plan changed');
        if(request.action.kind==='review')plan.seenRevision=plan.rev;
        return structuredClone(plan);
      });
      ipcMain.removeHandler('intercat:stage');ipcMain.handle('intercat:stage',(_event,request:WorkSubmission)=>{
        fixture.calls.push(request);
        const old=fixture.saved.get(request.opId);if(old)return old;
        if(request.planItem?.planId!==plan.id||request.planItem.itemId!==itemId||request.planItem.revision!==plan.rev||request.target.mode!=='lock'||request.target.trackId!=='A')throw new Error('Wrong item scope');
        const word:Utterance={id:request.opId,text:request.text,createdAt:Date.now(),attachments:[],target:request.target,state:'placed',revision:1,reason:null,
          planId:plan.id,planItemId:itemId,planContinuation:{title:plan.items[0].title},decision:{kind:'extend',trackId:'A',boundary:'after-turn',command:null,confidence:1,inferenceMs:0,timedOut:false}};
        fixture.saved.set(request.opId,word);state.utterances!.push(word);plan.rev++;state.sequence!++;
        if(fixture.lose){fixture.lose=false;throw new Error('Dummy saved acknowledgement lost');}
        return word;
      });
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(zoom);
    },{planId,itemId,zoom});
    const page=await app.firstWindow();const draft=page.getByRole('textbox',{name:'Request draft',exact:true});
    await draft.fill('Keep this original B draft.');
    await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
    const openItem=async()=>{
      await page.getByRole('region',{name:'Plans',exact:true}).getByRole('button',{name:'Review plan',exact:true}).click();
      await page.getByRole('button',{name:'Add instruction to Dummy A result',exact:true}).click();
    };
    await openItem();
    const context=page.getByLabel('Plan item for this instruction',{exact:true});
    await expect(context).toContainText('Dummy A result');await expect(context).toContainText('Worker A');
    await expect(draft).toHaveValue('');
    await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'New track',exact:true})).toBeDisabled();
    await draft.fill('Add the final dummy check.');
    await page.getByRole('button',{name:'Actions for Worker B',exact:true}).focus();
    await page.keyboard.press('Enter');
    await page.getByRole('menuitem',{name:'Say to B',exact:true}).click();
    await expect(draft).toHaveValue('Keep this original B draft.');
    await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
    await openItem();await expect(draft).toHaveValue('Add the final dummy check.');
    await context.getByRole('button',{name:'Back to draft',exact:true}).click();
    await expect(draft).toHaveValue('Keep this original B draft.');
    await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
    await openItem();await expect(draft).toHaveValue('Add the final dummy check.');
    await page.reload();await expect(context).toContainText('Worker A');await expect(draft).toHaveValue('Add the final dummy check.');
    await app.evaluate(()=>{const fixture=(globalThis as any).planContextFixture;fixture.state.plans[0].rev++;fixture.state.sequence++;});
    await expect(context).toContainText('The plan changed');
    await expect(page.getByRole('button',{name:'Send to item',exact:false})).toBeDisabled();
    await context.getByRole('button',{name:'Review item',exact:true}).click();
    await page.getByRole('button',{name:'Add instruction to Dummy A result',exact:true}).click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(context).not.toContainText('The plan changed');
    await expect(draft).toHaveValue('Add the final dummy check.');
    await writeFile(info.outputPath('item-context.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await page.getByRole('button',{name:'Send to item',exact:false}).click();
    await expect(page.getByRole('button',{name:/Retry save/})).toBeVisible();
    await page.getByRole('button',{name:/Retry save/}).click();
    await expect(draft).toHaveValue('Keep this original B draft.');
    await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
    const calls=await app.evaluate(()=>(globalThis as any).planContextFixture.calls as WorkSubmission[]);
    expect(calls).toHaveLength(2);expect(calls[0]).toEqual(calls[1]);
    expect(calls[0].planItem).toEqual({planId,itemId,revision:4});
    await expect(page.locator('.request-strip')).toContainText('About Dummy A result');
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
