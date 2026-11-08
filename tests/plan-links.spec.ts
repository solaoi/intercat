import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';
import { mockDrafts } from './support/drafts';
import type { Plan, PlanCommand } from '../src/plans/types';
import type { PlanDependencyLocation } from '../src/bridge';

const id='00000000-0000-5000-8000-000000000001';
const title='ダミーの長い日本語タイトル 🧑‍🚀 家族 👨‍👩‍👧‍👦 を途中で壊さず省略する';

for (const zoom of [1,2]) test(`Plan heads, selected dependencies and overflow stay usable at ${zoom*100}%`,async({},info)=>{
  const directory=await mkdtemp('/private/tmp/ic-plan-links-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  await mockDrafts(app);
  try{
    await app.evaluate(({ipcMain,BrowserWindow},{zoom,title})=>{
      const plan:Plan={id:'00000000-0000-5000-8000-000000000001',rev:2,state:'draft',sourceId:'source',sourceRevision:1,startMode:'manual',executeAllowed:false,lockedTrack:null,
        generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Dummy A → B → C plan',assumptions: [], notices:[],seenRevision:null,rewrite:null,
        items:['1','2','3','1','1','3'].map((track,index)=>({planItemId:`item-${index}`,trackRef:`new:${track}`,folderId:'dummy',order:index,title:index===0?title:`Dummy ${index}`,
          instruction:`Dummy instructions ${index}`,constraints:[],doneWhen:'Done.',after:index===1?[{id:'item-0',inherit:false}]:index===2?[{id:'item-1',inherit:false}]:index===4?[{id:'item-2',inherit:false}]:[],state:'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:null}))};
      const snapshot=()=>({connected:true,sequence:plan.rev,tracks:[],workers:[],utterances:[],plans:[plan],planDependencies:Object.fromEntries(plan.items.map(item=>[item.planItemId,{id:item.planItemId,planId:plan.id,itemId:item.planItemId,retryable:false,trackId:`${plan.id}/${item.trackRef}`,title:item.title,state:item.state,unclear:false,clipId:null,atMs:null} satisfies PlanDependencyLocation]))});
      Object.assign(globalThis,{linksFixture:{plan}});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',snapshot);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:plan-command');ipcMain.handle('intercat:plan-command',(_event,request:PlanCommand)=>{
        if(request.revision!==plan.rev)throw new Error('The plan changed');
        if(request.action.kind==='review')plan.seenRevision=plan.rev;
        if(request.action.kind==='cut'){const itemId=request.action.itemId;plan.items.find(item=>item.planItemId===itemId)!.state='cut';plan.rev++;plan.seenRevision=null;}
        return structuredClone(plan);
      });
      const win=BrowserWindow.getAllWindows()[0];win.setSize(zoom===1?1440:1040,zoom===1?1000:760);win.webContents.setZoomFactor(zoom);
    },{zoom,title});
    const page=await app.firstWindow();const timeline=page.getByRole('region',{name:'Timeline',exact:true});
    await expect(timeline).toHaveAttribute('data-ready','true');
    await expect(timeline).toHaveAttribute('data-future-count',zoom===1?'2':'1');
    if(zoom===2)await page.getByRole('separator',{name:'Resize stage and desk'}).press('End');
    const scroll=page.getByRole('scrollbar',{name:'All tracks'});
    const firsts=['item-0','item-1','item-2'].map(item=>page.locator(`[data-plan-item-id="${item}"] .planned-item-select`));
    const reveal=async(index:number)=>{await scroll.press('Home');for(let step=0;step<index*2;step++)await scroll.press('ArrowDown');await expect(firsts[index]).toBeAttached();};
    const x:number[]=[];
    for(let index=0;index<firsts.length;index++){await reveal(index);x.push(await firsts[index].evaluate(el=>el.getBoundingClientRect().left));}
    expect(Math.max(...x)-Math.min(...x)).toBeLessThan(1);
    await reveal(0);
    await expect(firsts[0]).toHaveAttribute('title',/ダミーの長い日本語タイトル/);
    await expect(timeline).toHaveAttribute('data-dependency-count','0');
    await reveal(1);await firsts[1].click({timeout:3000});const review=page.getByRole('dialog',{name:'Review plan',exact:true});
    await review.getByRole('button',{name:'Back to plans',exact:true}).click();
    await expect(review).toHaveCount(0);
    await expect(timeline).toHaveAttribute('data-dependency-count','2');
    await expect(page.getByRole('list',{name:'Selected plan dependencies'})).toContainText('Dummy 1 → Dummy 2');
    await page.evaluate(()=>new Promise<void>(resolve=>requestAnimationFrame(()=>requestAnimationFrame(()=>resolve()))));
    await page.screenshot({path:info.outputPath('dependencies.png'),fullPage:true});
    await writeFile(info.outputPath('dependencies-native.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    await reveal(0);
    const overflow=page.locator(`[data-plan-track-id="${id}/new:1"]`).getByRole('button',{name:/Show \d+ more planned items/});
    await overflow.click();const menu=page.getByRole('menu',{name:'More planned items for New 1'});
    await expect(menu.getByRole('menuitem',{name:/Dummy 4/})).toBeVisible();
    await expect(menu.getByRole('menuitem').first()).toBeFocused();await page.keyboard.press('End');
    await expect(menu.getByRole('menuitem',{name:/Dummy 4/})).toBeFocused();await page.keyboard.press('Enter');
    await expect(review.getByRole('heading',{name:'Dummy 4',exact:true})).toBeFocused();
    await review.getByRole('button',{name:'Back to plans',exact:true}).click();
    await expect(overflow).toHaveClass(/selected/);
    await expect(firsts[0]).toBeAttached();
    await expect(timeline).toHaveAttribute('data-dependency-count','1');
    await reveal(1);await firsts[1].click();await review.getByRole('button',{name:'Cut Dummy 1',exact:true}).click();
    await review.getByRole('button',{name:'Back to plans',exact:true}).click();
    await scroll.press('End');
    await firsts[2].click();await review.getByRole('button',{name:'Back to plans',exact:true}).click();
    await expect(firsts[2]).toHaveAttribute('title',/was cut/);
    await expect(timeline).toHaveAttribute('data-dependency-count','1');
    // C still leads to the hidden A item; the cut B → C edge is absent.
    await expect(page.getByRole('list',{name:'Selected plan dependencies'})).not.toContainText('Dummy 1 → Dummy 2');
  }finally{
    const page=await app.firstWindow();
    await writeFile(info.outputPath('last-layout.json'),JSON.stringify(await page.evaluate(()=>Object.fromEntries(['.app','.stage','.timeline','.timeline-scroll','.desk','.stage-panel-controls','[data-plan-item-id="item-1"]'].map(selector=>{const e=document.querySelector(selector);return [selector,e?{rect:e.getBoundingClientRect().toJSON(),scrollTop:e.scrollTop,clientHeight:e.clientHeight,scrollHeight:e.scrollHeight}:null];}))),null,2));
    await page.screenshot({path:info.outputPath('last-screen.png')});
    await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
