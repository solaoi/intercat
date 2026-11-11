import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';

for(const zoom of [1,2])test(`project selection choices are scoped and readable at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-project-selection-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const roots=Array.from({length:8},(_,index)=>`/dummy/agent-created/long-root-${index}/repository-${index}`);
      const selection={id:'dummy-selection-1',issue:{kind:'duplicate',roots},error:null,mayHaveSaved:false};
      const state:any={connected:true,sequence:1,project:null,projectSelection:null,tracks:[],workers:[],utterances:[],workspaces:[],editor:{phase:'ready',accepting:true},privacy:{mode:'basic',revision:0}};
      const fixture={state,selection,choices:[] as any[],setups:0};Object.assign(globalThis,{projectSelectionFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:project-setup');ipcMain.handle('intercat:project-setup',()=>{fixture.setups++;state.projectSelection=structuredClone(fixture.selection);state.sequence++;return{project:null,selection:state.projectSelection};});
      ipcMain.removeHandler('intercat:project-selection');ipcMain.handle('intercat:project-selection',(_event,request)=>{
        fixture.choices.push(request);
        if(request.action==='useRoot') {
          if(fixture.choices.length===1)throw new Error('Dummy lost choice reply');
          state.project={id:'dummy-project',name:'Dummy',folders:[{id:'dummy-root',name:'Dummy root',root:roots[0],path:roots[0]}]};
        }
        state.projectSelection=null;state.sequence++;return{project:state.project};
      });
      BrowserWindow.getAllWindows()[0].setSize(1040,760);BrowserWindow.getAllWindows()[0].webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();const card=page.locator('.project-selection');
    const release=async()=>{await page.getByRole('textbox',{name:'Request draft'}).focus();await page.mouse.move(5,5);};
    const capture=async(name:string)=>{
      const output=path.join(process.cwd(),'research/verify/m4-project-selection-ui-20261109-01');await mkdir(output,{recursive:true});
      const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));
      await writeFile(path.join(output,`${name}-${zoom*100}.png`),Buffer.from(png,'base64'));
    };
    await page.getByRole('button',{name:'Choose project folder',exact:true}).click();
    await expect(card).toContainText('Same repository already added');
    await card.locator('summary').click();await expect(card.locator('details')).toHaveJSProperty('open',true);await expect(card.locator('li').first()).toBeVisible();await expect(card.locator('li')).toHaveCount(8);
    await capture('duplicate');
    const bounds=await page.locator('.project-selection, .project-selection strong, .project-selection .desk-actions button').evaluateAll(elements=>elements.map(element=>{const rect=element.getBoundingClientRect();const viewport=element.closest('.desk-scroll, .stage-cards')?.getBoundingClientRect();return{top:rect.top,bottom:rect.bottom,left:rect.left,right:rect.right,viewTop:viewport?.top??0,viewBottom:viewport?.bottom??innerHeight,width:innerWidth};}));
    for(const bound of bounds){expect(bound.top).toBeGreaterThanOrEqual(bound.viewTop-1);expect(bound.bottom).toBeLessThanOrEqual(bound.viewBottom+1);expect(bound.left).toBeGreaterThanOrEqual(0);expect(bound.right).toBeLessThanOrEqual(bound.width+1);}
    const roots=card.getByRole('list',{name:'Repository roots',exact:true});
    await roots.focus();await roots.press('End');await expect.poll(()=>roots.evaluate(element=>element.scrollTop)).toBeGreaterThan(0);
    await roots.press('Home');await expect.poll(()=>roots.evaluate(element=>element.scrollTop)).toBe(0);
    await card.getByRole('button',{name:'Use the root',exact:true}).click();
    await expect(card).toContainText('Dummy lost choice reply');
    await card.getByRole('button',{name:'Retry saving folder choice',exact:true}).click();await release();
    await expect(card).toHaveCount(0);
    for(const kind of ['ignored','submodules']) {
      await app.evaluate((_electron,kind)=>{const f=(globalThis as any).projectSelectionFixture;f.state.project=null;f.selection={id:`dummy-${kind}`,issue:{kind,path:'/dummy/root/selected',root:'/dummy/root'},error:null,mayHaveSaved:false};f.state.sequence++;},kind);
      await page.getByRole('button',{name:'Choose project folder',exact:true}).click();
      await expect(card).toContainText(kind==='ignored'?'Ignored by its repository':'Repositories with submodules aren’t supported yet');
      await expect(card.getByRole('button',{name:'Use the root',exact:true})).toHaveCount(0);
      await expect(card.getByRole('button',{name:'Cancel',exact:true})).toBeEnabled();await capture(kind);
      await page.reload();await expect(card.getByRole('button',{name:'Cancel',exact:true})).toBeEnabled();
      await card.getByRole('button',{name:'Cancel',exact:true}).click();await release();
      await expect(card).toHaveCount(0);
    }
    const choices=await app.evaluate(()=>(globalThis as any).projectSelectionFixture.choices);
    expect(choices.map((choice:any)=>choice.action)).toEqual(['useRoot','useRoot','cancel','cancel']);expect(choices[0]).toEqual(choices[1]);
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
