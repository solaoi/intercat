import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { mockDrafts } from './support/drafts';
import { closeWindow } from './support/close';
import type { DaemonStatus, HistoryRequest } from '../src/bridge';
import type { Plan, PlanCommand } from '../src/plans/types';

const planId='00000000-0000-5000-8000-000000000051',itemId='00000000-0000-5000-8000-000000000052';
for(const zoom of [1,2]) test(`A locked plan move needs consent and can be undone at ${zoom*100}%`,async({},info)=>{
  const directory=await mkdtemp('/private/tmp/ic-unlock-move-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  try{
    await app.evaluate(({ipcMain,BrowserWindow},{zoom,planId,itemId})=>{
      const plan:Plan={id:planId,rev:1,state:'draft',sourceId:'source',sourceRevision:1,executeAllowed:false,lockedTrack:'A',startMode:'manual',
        generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Dummy locked plan',assumptions: [], notices:[],seenRevision:null,rewrite:null,
        items:[0,1].map(index=>({planItemId:index===0?itemId:'other-item',trackRef:'A',folderId:'dummy',order:index,title:`Dummy move ${index}`,instruction:'Dummy only',constraints:[],doneWhen:'Done',after:[],state:'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:null}))};
      const state:DaemonStatus={connected:true,workerConfigured:true,sequence:1,plans:[plan],workers:[],utterances:[],tracks:[{id:'A',label:'A',title:'Dummy A',status:'done'},{id:'B',label:'B',title:'Dummy B',status:'done'}],editHistory:{undo:null,redo:null,takenBack:[]}};
      const fixture={state,calls:[] as PlanCommand[],histories:[] as HistoryRequest[],receipts:new Set<string>(),before:null as Plan|null,after:null as Plan|null};
      Object.assign(globalThis,{unlockMoveFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:plan-command');ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
        fixture.calls.push(request);if(fixture.receipts.has(request.opId))return structuredClone(state.plans![0]);
        if(request.planId!==planId||request.revision!==state.plans![0].rev)throw new Error('Stale plan');
        const plan=state.plans![0];
        if(request.action.kind==='review'){plan.seenRevision=plan.rev;return structuredClone(plan);}
        if(request.action.kind!=='unlock-and-move'||request.action.itemId!==itemId||request.action.trackId!=='B'||plan.lockedTrack!=='A')throw new Error('Wrong consented move');
        fixture.before=structuredClone(plan);fixture.receipts.add(request.opId);
        plan.items[0].trackRef='B';plan.items[0].order=2;plan.lockedTrack=null;plan.rev++;plan.sourceRevision++;plan.seenRevision=null;
        fixture.after=structuredClone(plan);state.editHistory!.undo={entryId:'move-history',label:'Unlock and move plan item',text:'Dummy move 0',at:0,blocked:null};state.sequence!++;
        throw new Error('Dummy acknowledgement lost');
      });
      ipcMain.removeHandler('intercat:history');ipcMain.handle('intercat:history',(_event,request:HistoryRequest)=>{
        fixture.histories.push(request);const redo=request.action.kind==='redo';
        const next=structuredClone(redo?fixture.after!:fixture.before!);next.rev=state.plans![0].rev+1;next.sourceRevision=state.plans![0].sourceRevision+1;state.plans=[next];state.sequence!++;
        const entry={entryId:'move-history',label:'Unlock and move plan item',text:'Dummy move 0',at:0,blocked:null};state.editHistory={undo:redo?entry:null,redo:redo?null:entry,takenBack:[]};return state;
      });
      BrowserWindow.getAllWindows()[0].setSize(1040,760);BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    },{zoom,planId,itemId});
    const page=await app.firstWindow(),draft=page.getByRole('textbox',{name:'Request draft',exact:true});
    await draft.fill('Keep this independent dummy draft.');await page.getByRole('combobox',{name:'Request destination'}).selectOption('B');
    const tile=()=>page.locator(`[data-plan-item-id="${itemId}"] .planned-item-select`);
    await tile().press('Alt+ArrowDown');
    const consent=page.getByRole('dialog',{name:'Unlock and split?',exact:true});
    await expect(consent).toBeVisible();await expect(consent).toContainText('from A to B');
    expect(await app.evaluate(()=>(globalThis as any).unlockMoveFixture.calls)).toEqual([]);
    await consent.getByRole('button',{name:'Keep on A',exact:true}).click();await expect(consent).toHaveCount(0);
    await tile().click();const review=page.getByRole('dialog',{name:'Review plan',exact:true});await expect(review).toBeVisible();
    const openMove=async()=>{await review.getByRole('button',{name:'Actions for Dummy move 0',exact:true}).click();const menu=page.getByRole('menu',{name:'Plan item actions for Dummy move 0',exact:true});await menu.getByRole('menuitem',{name:'Move to track…',exact:true}).click();await menu.getByRole('menuitem',{name:'Move to B',exact:true}).click();};
    await openMove();await expect(consent).toBeVisible();
    await app.evaluate(()=>{const state=(globalThis as any).unlockMoveFixture.state;state.workers=[{trackId:'B',phase:'lost',activeTurn:null,prompts:[],approvals:[],detail:null,sessionId:null,identity:null,output:null,checkpointPending:false,checkpoint:null}];state.sequence++;});
    await expect(consent.getByRole('button',{name:'Yes',exact:true})).toBeDisabled();
    await app.evaluate(()=>{const state=(globalThis as any).unlockMoveFixture.state;state.workers=[];state.sequence++;});
    await expect(consent.getByRole('button',{name:'Yes',exact:true})).toBeEnabled();
    await app.evaluate(()=>{const state=(globalThis as any).unlockMoveFixture.state;state.plans[0].rev++;state.sequence++;});
    await expect(consent.getByRole('button',{name:'Yes',exact:true})).toBeDisabled();await expect(consent).toContainText('The plan changed');
    await consent.getByRole('button',{name:'Review current plan',exact:true}).click();await expect(consent).toHaveCount(0);
    await expect(review.getByText('The plan changed · review again',{exact:false})).toHaveCount(0);
    await openMove();await expect(consent.getByRole('button',{name:'Yes',exact:true})).toBeEnabled();
    for(const name of ['Yes','Keep on A'])await expect(consent.getByRole('button',{name,exact:true})).toBeInViewport();
    await writeFile(info.outputPath('unlock-move.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await consent.getByRole('button',{name:'Yes',exact:true}).click();await expect(consent).toContainText('Dummy acknowledgement lost');
    await consent.getByRole('button',{name:'Retry plan action',exact:true}).click();await expect(consent).toHaveCount(0);
    await review.getByRole('button',{name:'Close plan review',exact:true}).click();
    await expect(draft).toHaveValue('Keep this independent dummy draft.');await expect(page.getByRole('combobox',{name:'Request destination'})).toHaveValue('B');
    const result=await app.evaluate(()=>(globalThis as any).unlockMoveFixture);
    const moves=result.calls.filter((call:PlanCommand)=>call.action.kind==='unlock-and-move');expect(moves).toHaveLength(2);expect(moves[0]).toEqual(moves[1]);
    expect(result.state.plans[0].lockedTrack).toBeNull();expect(result.state.plans[0].items[0].trackRef).toBe('B');expect(result.state.plans[0].items[1].trackRef).toBe('A');
    await expect(page.getByRole('button',{name:'Undo',exact:true})).toBeEnabled();
    await page.getByRole('scrollbar',{name:'All tracks',exact:true}).focus();await page.keyboard.press('Meta+z');
    await expect.poll(()=>app.evaluate(()=>(globalThis as any).unlockMoveFixture.state.plans[0].lockedTrack)).toBe('A');
    await expect(page.getByRole('button',{name:'Redo',exact:true})).toBeEnabled();
    await page.keyboard.press('Meta+Shift+z');await expect.poll(()=>app.evaluate(()=>(globalThis as any).unlockMoveFixture.state.plans[0].lockedTrack)).toBeNull();
    // The dependency menu uses the same central consent path.
    await app.evaluate(()=>{const fixture=(globalThis as any).unlockMoveFixture,state=fixture.state;const plan=structuredClone(fixture.before);plan.rev=state.plans[0].rev+1;plan.state='held';plan.items[0].after=[{id:'external-clip',inherit:false}];state.plans=[plan];state.sequence++;state.planDependencies={'external-clip':{id:'external-clip',planId:null,itemId:null,retryable:false,trackId:'B',title:'Dummy parent',state:'failed',unclear:false,clipId:null,atMs:null}};});
    await expect(tile()).toHaveAttribute('aria-haspopup','menu');await tile().click();
    const dependency=page.getByRole('menu',{name:'Dependency actions for Dummy move 0',exact:true});
    await app.evaluate(()=>{const state=(globalThis as any).unlockMoveFixture.state;state.tracks[1].status='lost';state.sequence++;});
    await dependency.getByRole('menuitem',{name:'Move to track…',exact:true}).click();
    await expect(dependency.getByRole('menuitem',{name:'Move to B',exact:true})).toHaveCount(0);
    await app.evaluate(()=>{const state=(globalThis as any).unlockMoveFixture.state;state.tracks[1].status='done';state.sequence++;});
    await dependency.getByRole('menuitem',{name:'Move to B',exact:true}).click();
    await expect(consent).toBeVisible();await consent.getByRole('button',{name:'Keep on A',exact:true}).click();
    expect((await app.evaluate(()=>(globalThis as any).unlockMoveFixture.calls)).filter((call:PlanCommand)=>call.action.kind==='unlock-and-move')).toHaveLength(2);
  }finally{await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
