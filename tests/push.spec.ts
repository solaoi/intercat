import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';
for(const zoom of [1,2])test(`Push requires review and explicit confirmation and retains a lost reply at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-push-ui-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try {
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const folders=['web','api'].map((name,index)=>({id:`dummy-folder-${name}`,name:`Dummy ${name}`,path:`/dummy/${name}`,root:`/dummy/${name}`,commonDir:`/dummy/${name}/.git`,base:'c'.repeat(40),baseBranch:'main'}));
      const jobs=folders.map((project,index)=>({id:`dummy-integration-${index}`,revision:1,status:'done',project,sessionName:'Own dummy Push',slug:'dummy-push',tracks:[],plan:null,candidate:null,error:null,messages:{},keptSecrets:[],contributions:{},assumptions:[],assumptionsChanged:false,assumptionsPending:0,assumptionsFailed:0,orderReview:null,pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions:[]}));
      const state:any={connected:true,sequence:1,project:{id:'dummy-project',name:'Own dummy Push',folders},session:{id:'dummy-session',name:'Own dummy Push',bound:true,slug:'dummy-push'},integrations:jobs,pushes:[],workers:[],workspaces:[],tracks:[],utterances:[],repairRuns:[],workerConfigured:false};
      const fixture:any={state,requests:[],commands:[],presentations:new Map(),lost:false};Object.assign(globalThis,{pushFixture:fixture});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>structuredClone(state));
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',(_event,bounds)=>({revision:state.sequence,reset:true,clips:[],marks:fixture.mark?[{id:'own-dummy-commit-mark',trackId:null,repairRunId:null,at:Math.floor((bounds.start+bounds.end)/2),inputId:null,text:'Committed',operation:'command',command:'commit',commit:{folderId:folders[0].id,sha:'a'.repeat(40),jobId:jobs[0].id,pushed:true}}]:[],removed:[],removedMarks:[]}));
      ipcMain.removeHandler('intercat:push-remotes');ipcMain.handle('intercat:push-remotes',()=>['origin','own-backup']);
      const destination=(name:string)=>({fingerprint:name.padEnd(64,'0'),display:`https://${name}.example.invalid/dummy/repository.git`,credentials:false});
      ipcMain.removeHandler('intercat:push-review');ipcMain.handle('intercat:push-review',(_event,request)=>{
        fixture.requests.push(request);
        const review={request,projectId:state.project.id,sessionId:state.session.id,sessionName:state.session.name,members:request.choices.map((choice:any,index:number)=>{
          const target={sourceRef:'refs/heads/intercat/dummy-push',targetRef:'refs/heads/intercat/dummy-push',sha:(index?'b':'a').repeat(40),remote:choice.remote,destinations:index?[destination('own-api')]:[destination('own-web'),destination('own-backup')]};
          return{folder:folders.find(folder=>folder.id===choice.folderId),slug:'dummy-push',target,lfs:target.destinations.map(git=>({git,upload:destination('own-lfs'),api:destination('own-lfs')})),ssh:[]};
        })};
        const presentation={review,commits:Object.fromEntries(review.members.map((member:any)=>[member.folder.id,[member.target.sha]])),additionalCommits:Object.fromEntries(review.members.map((member:any)=>[member.folder.id,0]))};fixture.presentations.set(request.opId,presentation);return structuredClone(presentation);
      });
      ipcMain.removeHandler('intercat:push-presentation');ipcMain.handle('intercat:push-presentation',(_event,{id})=>structuredClone(fixture.presentations.get(id)));
      ipcMain.removeHandler('intercat:utterance-read');ipcMain.handle('intercat:utterance-read',(_event,id)=>structuredClone(state.utterances.find((word:any)=>word.id===id)));
      ipcMain.removeHandler('intercat:resolve');ipcMain.handle('intercat:resolve',(_event,request)=>{
        const word=state.utterances.find((word:any)=>word.id===request.utteranceId);
        if(!word||request.resolution.action!=='presented'||request.opId!==word.presentation.receiptId)throw new Error('Wrong dummy presentation receipt');
        word.presentation=null;word.state='landed';word.revision++;state.sequence++;return structuredClone(word);
      });
      ipcMain.removeHandler('intercat:push-command');ipcMain.handle('intercat:push-command',(_event,request)=>{
        fixture.commands.push(request);
        if(request.action.action==='confirm'){
          const old=state.pushes.find((job:any)=>job.review.request.opId===request.action.review);if(old)return structuredClone(old);
          const presentation=fixture.presentations.get(request.action.review);
          const job={id:request.opId,revision:1,review:presentation.review,stopping:false,recipients:presentation.review.members.flatMap((member:any)=>member.target.destinations.map((destination:any,index:number)=>({folderId:member.folder.id,destination,phase:index===1?'uncertain':'done',attempt:0,scans:[],receipt:index===1?null:{sha:member.target.sha,reference:member.target.targetRef,destination,changed:true},reason:index===1?'Own dummy lost send result':null})))};
          state.pushes.push(job);state.sequence++;
          fixture.lost=true;throw new Error('Dummy acknowledgement lost');
        }
        const job=state.pushes.find((job:any)=>job.id===request.action.id);if(!job||job.revision!==request.action.revision)throw new Error('Stale dummy Push');
        if(request.action.action!=='retry')throw new Error('Unexpected dummy action');
        for(const recipient of job.recipients){if(recipient.phase==='done')continue;recipient.phase='done';recipient.attempt++;recipient.reason=null;const member=job.review.members.find((member:any)=>member.folder.id===recipient.folderId);recipient.receipt={sha:member.target.sha,reference:member.target.targetRef,destination:recipient.destination,changed:false};}
        fixture.presentations.get(job.review.request.opId).additionalCommits['dummy-folder-web']=1;job.revision++;state.sequence++;return structuredClone(job);
      });
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();await page.reload();
    await page.getByRole('button',{name:'Push · 2 folders',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Push review',exact:true});
    await expect(sheet.getByRole('button',{name:'Review Push',exact:true})).toBeEnabled();
    expect(await app.evaluate(()=>(globalThis as any).pushFixture.commands)).toHaveLength(0);
    await sheet.getByRole('button',{name:'Review Push',exact:true}).click();
    await expect(sheet.getByRole('button',{name:'Push to 3 destinations',exact:true})).toBeEnabled();
    await expect(sheet.getByText('Files may go to storage locations provided by this LFS service',{exact:true})).toHaveCount(2);
    expect(await app.evaluate(()=>(globalThis as any).pushFixture.commands)).toHaveLength(0);
    const output=path.join(process.cwd(),'research/verify/m4-push-ui-20261109-01');await mkdir(output,{recursive:true});
    const capture=async(name:string)=>{
      const layout=await sheet.locator('.push-footer').evaluate(element=>{const box=element.getBoundingClientRect();return{top:box.top,bottom:box.bottom,viewport:innerHeight};});expect(layout.top).toBeGreaterThan(0);expect(layout.bottom).toBeLessThanOrEqual(layout.viewport+1);
      const overflow=await sheet.evaluate(element=>element.scrollWidth>element.clientWidth+1);expect(overflow).toBe(false);
      const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));await writeFile(path.join(output,`${name}-${zoom*100}.png`),Buffer.from(png,'base64'));
    };
    const confirm=sheet.getByRole('button',{name:'Push to 3 destinations',exact:true});
    await confirm.hover();
    const contrast=await confirm.evaluate(element=>{const style=getComputedStyle(element);const luminance=(css:string)=>{const rgb=css.match(/\d+/g)!.slice(0,3).map(Number).map(value=>{const channel=value/255;return channel<=.04045?channel/12.92:((channel+.055)/1.055)**2.4;});return rgb[0]*.2126+rgb[1]*.7152+rgb[2]*.0722;};const foreground=luminance(style.color),background=luminance(style.backgroundColor);return (Math.max(foreground,background)+.05)/(Math.min(foreground,background)+.05);});
    expect(contrast).toBeGreaterThanOrEqual(4.5);
    await capture('review');
    await sheet.getByRole('button',{name:'Push to 3 destinations',exact:true}).click();
    await expect(sheet.getByText(/Dummy acknowledgement lost/)).toBeVisible();
    await sheet.getByRole('button',{name:'Retry saved choice',exact:true}).click();
    await expect(sheet.getByRole('button',{name:'Retry unfinished destinations',exact:true})).toBeEnabled();
    const first=await app.evaluate(()=>(globalThis as any).pushFixture.commands);expect(first).toHaveLength(2);expect(first[0]).toEqual(first[1]);
    await capture('partial');
    await sheet.getByRole('button',{name:'Retry unfinished destinations',exact:true}).click();
    await expect(sheet.getByText('1 commit added after you reviewed · not pushed',{exact:true})).toBeVisible();
    await capture('done');
    const done=await app.evaluate(()=>(globalThis as any).pushFixture.state.pushes[0]);expect(done.recipients.map((recipient:any)=>recipient.attempt)).toEqual([0,1,0]);expect(done.recipients.every((recipient:any)=>recipient.phase==='done')).toBe(true);
    await sheet.getByRole('button',{name:'Hide Push review',exact:true}).click();
    await page.getByLabel('More project actions',{exact:true}).click();await page.getByRole('button',{name:'Pushed · Dummy web, Dummy api',exact:true}).click();
    await expect(sheet.getByText('1 commit added after you reviewed · not pushed',{exact:true})).toBeVisible();
    await sheet.getByRole('button',{name:'Hide Push review',exact:true}).click();
    await app.evaluate(()=>{const fixture=(globalThis as any).pushFixture;fixture.mark=true;fixture.state.sequence++;});
    if(zoom===2)await page.keyboard.press('Meta+Shift+T');
    await page.getByRole('button',{name:'Committed · Push to here · aaaaaaaa',exact:true}).click();
    await sheet.getByRole('button',{name:'Review Push',exact:true}).click();
    const through=await app.evaluate(()=>(globalThis as any).pushFixture.requests.at(-1));expect(through.choices).toHaveLength(1);expect(through.choices[0].folderId).toBe('dummy-folder-web');expect(through.choices[0].through).toBe('a'.repeat(40));
    expect(await app.evaluate(()=>(globalThis as any).pushFixture.commands)).toHaveLength(3);
    await sheet.getByRole('button',{name:'Hide Push review',exact:true}).click();
    await app.evaluate(()=>{const fixture=(globalThis as any).pushFixture;fixture.state.utterances=[{id:'own-dummy-push-word',text:'Push the saved work',state:'placed',revision:1,createdAt:Date.now(),target:{mode:'none'},attachments:[],reason:null,decision:{kind:'command',command:'push',trackId:null,boundary:null},presentation:{command:'push',operationId:'01a1bdca-b623-72ad-947d-9001299b2260',receiptId:'01a1bdca-b623-72ad-947d-9001299b2261',trackId:null}}];fixture.state.sequence++;});
    await expect(sheet.getByRole('button',{name:'Review Push',exact:true})).toBeEnabled();
    await expect.poll(()=>app.evaluate(()=>(globalThis as any).pushFixture.state.utterances[0].state)).toBe('landed');
    expect(await app.evaluate(()=>(globalThis as any).pushFixture.commands)).toHaveLength(3);
  } finally {await closeWindow(app);await rm(directory,{recursive:true,force:true});}
});
