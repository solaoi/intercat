import {_electron as electron,expect,test} from '@playwright/test';
import {mkdir,mkdtemp,rm,writeFile} from 'node:fs/promises';
import path from 'node:path';
import {mockDrafts} from './support/drafts';
import {closeWindow} from './support/close';
for(const zoom of [1,2])test(`PR review keeps explicit confirmations and saved partial results at ${zoom*100}%`,async()=>{
  const directory=await mkdtemp('/private/tmp/ic-pr-ui-');
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+directory]});
  try{
    await mockDrafts(app);
    await app.evaluate(({ipcMain,BrowserWindow},zoom)=>{
      const folders=['web','api'].map(name=>({id:`dummy-${name}`,name:`Dummy ${name}`,path:`/dummy/${name}`,root:`/dummy/${name}`,commonDir:`/dummy/${name}/.git`,base:'c'.repeat(40),baseBranch:'refs/heads/main' as string|null}));
      const members=folders.map((folder,index)=>{const destination={fingerprint:(index?'b':'a').repeat(64),display:`https://github.com/own-dummy/${index?'api':'web'}.git`,credentials:false};return{folder,slug:`own-pr-${index}`,target:{sourceRef:`refs/heads/intercat/own-pr-${index}`,targetRef:`refs/heads/intercat/own-pr-${index}`,sha:(index?'b':'a').repeat(40),remote:'origin',destinations:[destination]},lfs:[{git:destination,upload:destination,api:destination}],ssh:[]};});
      const review={request:{opId:'01a1bdca-b623-72ad-947d-9001299b225e',choices:members.map(member=>({folderId:member.folder.id,remote:'origin',targetRef:null,through:null}))},projectId:'own-dummy-project',sessionId:'own-dummy-session',sessionName:'Own PR test',members};
      const pushed={id:'own-dummy-push',revision:1,review,stopping:false,recipients:members.map(member=>({folderId:member.folder.id,destination:member.target.destinations[0],phase:'done',attempt:0,scans:[],receipt:{sha:member.target.sha,reference:member.target.targetRef,destination:member.target.destinations[0],changed:true},reason:null}))};
      const state:any={connected:true,sequence:1,project:{id:review.projectId,name:'Own PR test',folders},session:{id:review.sessionId,name:'Own PR test',bound:true,slug:'own-pr'},integrations:members.map(member=>({id:`commit-${member.folder.id}`,revision:1,status:'done',project:member.folder,sessionName:'Own PR test',slug:member.slug,tracks:[],candidate:{head:member.target.sha},messages:{},keptSecrets:[],contributions:{},assumptions:[],assumptionsPending:0,assumptionsFailed:0,orderReview:null,pathReview:{families:[],choices:{}},pathState:null,retirements:[],exclusions:[]})),pushes:[pushed],pullRequests:[],workers:[],workspaces:[],tracks:[],utterances:[],repairRuns:[],workerConfigured:false};
      const fixture:any={state,opens:[],commands:[],pushes:[],creates:0,lost:false,noBase:false,links:[],reviews:[],proposals:[]};Object.assign(globalThis,{pullFixture:fixture});
      for(const channel of ['status','timeline-window','push-remotes','push-review','push-command','pull-open','pull-command','pull-read','pull-github','proposal-answer','utterance-read','resolve'])ipcMain.removeHandler(`intercat:${channel}`);
      ipcMain.handle('intercat:status',()=>structuredClone(state));
      ipcMain.handle('intercat:timeline-window',()=>({revision:state.sequence,reset:true,clips:[],marks:[],removed:[],removedMarks:[]}));
      ipcMain.handle('intercat:push-remotes',()=>['origin']);
      ipcMain.handle('intercat:push-review',(_event,request)=>{fixture.reviews.push(request);const value=structuredClone(review);value.request=request;value.members=request.choices.map((choice:any)=>{const member=structuredClone(members.find(member=>member.folder.id===choice.folderId)!);if(choice.through)member.target.sha=choice.through;return member;});if(fixture.noBase)value.members[0].folder.baseBranch=null;return{review:value,commits:Object.fromEntries(members.map(member=>[member.folder.id,[member.target.sha]])),additionalCommits:{}};});
      ipcMain.handle('intercat:push-command',(_event,request)=>{fixture.pushes.push(request);throw new Error('Already pushed dummy must not send again');});
      ipcMain.handle('intercat:pull-open',(_event,request)=>{
        fixture.opens.push(request);
        const job={id:request.opId,revision:1,request,push:{...structuredClone(review),request:{...review.request,opId:request.pushReview}},account:{id:42,login:'own-dummy'},confirmed:null,held:false,members:members.map((member,index)=>({folderId:member.folder.id,name:member.folder.name,destination:member.target.destinations[0].fingerprint,target:{repository:{owner:'own-dummy',name:index?'api':'web'},base:'main',head:`intercat/own-pr-${index}`,sha:member.target.sha},draft:{title:`Own change ${index}`,body:`Own generated description ${index}\nValidation: recorded dummy check`},existing:null,result:null,phase:'review',error:null}))};state.pullRequests.push(job);state.sequence++;return structuredClone(job);
      });
      ipcMain.handle('intercat:pull-github',(_event,request)=>{fixture.links.push(request);});
      ipcMain.handle('intercat:pull-read',(_event,{id})=>structuredClone(state.pullRequests.find((job:any)=>job.id===id)));
      ipcMain.handle('intercat:pull-command',(_event,request)=>{
        fixture.commands.push(request);const action=request.action;const job=state.pullRequests.find((job:any)=>job.id===action.id);if(!job)throw new Error('Missing own PR');
        if(action.action==='confirm'&&job.confirmed)return structuredClone(job);
        if(job.revision!==action.revision)throw new Error('Stale own PR');
        if(action.action==='edit'){for(const edit of action.drafts){const member=job.members.find((member:any)=>member.folderId===edit.folderId);member.draft={title:edit.draft.title,body:edit.draft.body.replaceAll('own-private-marker','[redacted]')};}}
        else if(action.action==='confirm'){
          fixture.creates++;job.confirmed=request.opId;job.held=true;job.members[0].phase='created';job.members[0].result={number:7,url:'https://github.com/own-dummy/web/pull/7',headSha:'a'.repeat(40),state:'open'};job.members[1].phase='uncertain';job.members[1].error='Own dummy creation reply lost';job.revision++;state.sequence++;fixture.lost=true;throw new Error('Own dummy confirmation reply lost');
        }else if(action.action==='retry'){job.held=false;for(const[index,member]of job.members.entries()){member.phase='done';member.error=null;member.result={number:7+index,url:`https://github.com/own-dummy/${index?'api':'web'}/pull/${7+index}`,headSha:member.target.sha,state:'open'};}}
        else throw new Error('Unexpected own PR action');
        job.revision++;state.sequence++;return structuredClone(job);
      });
      ipcMain.handle('intercat:proposal-answer',(_event,request)=>{
        fixture.proposals.push(request);const proposal=state.proposal;if(proposal?.id!==request.proposalId)throw new Error('Wrong own publication proposal');
        const word={id:'own-publication-word',text:'Review Push and PR for committed changes',state:'placed',revision:1,createdAt:Date.now(),target:{mode:'none'},attachments:[],reason:null,decision:{kind:'command',command:'open-pr',trackId:null,boundary:null},presentation:{command:'open-pr',operationId:'01a1bdca-b623-72ad-947d-9001299b2270',receiptId:'01a1bdca-b623-72ad-947d-9001299b2271',trackId:null,publication:proposal.source.selection}};
        state.utterances.push(word);state.proposal=null;state.sequence++;return{proposalId:request.proposalId,action:request.action,inputId:word.id};
      });
      ipcMain.handle('intercat:utterance-read',(_event,id)=>structuredClone(state.utterances.find((word:any)=>word.id===id)));
      ipcMain.handle('intercat:resolve',(_event,request)=>{const word=state.utterances.find((word:any)=>word.id===request.utteranceId);if(request.resolution.action!=='presented')throw new Error('Wrong receipt');word.presentation=null;word.state='landed';word.revision++;state.sequence++;return structuredClone(word);});
      const window=BrowserWindow.getAllWindows()[0];window.setSize(1120,800);window.webContents.setZoomFactor(zoom);
    },zoom);
    const page=await app.firstWindow();await page.reload();
    await page.getByRole('button',{name:'Open PR · 2 folders',exact:true}).click();
    const push=page.getByRole('complementary',{name:'Push review',exact:true});
    await push.getByRole('button',{name:'Review Push',exact:true}).click();
    await push.getByRole('button',{name:'Continue to PR review',exact:true}).click();
    const sheet=page.getByRole('complementary',{name:'Pull request review',exact:true});
    await expect(sheet.getByRole('button',{name:'Prepare PRs',exact:true})).toBeEnabled();
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.opens)).toHaveLength(0);
    await sheet.getByRole('button',{name:'Prepare PRs',exact:true}).click();
    await expect(sheet.getByText('GitHub · @own-dummy',{exact:true})).toBeVisible();
    await expect(sheet.getByRole('button',{name:'Create or update 2 PRs',exact:true})).toBeEnabled();
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.commands)).toHaveLength(0);
    const body=sheet.getByRole('textbox',{name:'PR description for Dummy web',exact:true});
    await body.fill('Own reviewed body with own-private-marker');
    await expect(sheet.getByRole('button',{name:'Create or update 2 PRs',exact:true})).toHaveCount(0);
    await sheet.getByRole('button',{name:'Save text for review',exact:true}).click();
    await expect(body).toHaveValue('Own reviewed body with [redacted]');
    const output=path.join(process.cwd(),'research/verify/m4-open-pr-ui-20261109-01');await mkdir(output,{recursive:true});
    const capture=async(name:string)=>{const layout=await sheet.evaluate(element=>{const footer=element.querySelector('.push-footer')!.getBoundingClientRect();return{overflow:element.scrollWidth>element.clientWidth+1,top:footer.top,bottom:footer.bottom,height:innerHeight};});expect(layout.overflow).toBe(false);expect(layout.top).toBeGreaterThan(0);expect(layout.bottom).toBeLessThanOrEqual(layout.height+1);const png=await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].webContents.capturePage()).toPNG().toString('base64'));await writeFile(path.join(output,`${name}-${zoom*100}.png`),Buffer.from(png,'base64'));};
    await capture('review');
    await sheet.getByRole('button',{name:'Create or update 2 PRs',exact:true}).click();
    await expect(sheet.getByRole('button',{name:'Retry saved choice',exact:true})).toBeVisible();
    await sheet.getByRole('button',{name:'Retry saved choice',exact:true}).click();
    await expect(sheet.getByText('Creation result unknown',{exact:true})).toBeVisible();await capture('partial');
    await sheet.getByRole('button',{name:'Check existing result',exact:true}).click();
    await expect(sheet.getByText('PRs ready',{exact:true})).toBeVisible();await capture('done');
    await sheet.getByRole('button',{name:'View on GitHub',exact:true}).first().click();
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.links)).toHaveLength(1);
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.creates)).toBe(1);
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.pushes)).toHaveLength(0);
    await sheet.getByRole('button',{name:'Hide PR review',exact:true}).click();
    await app.evaluate(()=>{const fixture=(globalThis as any).pullFixture;fixture.state.utterances=[{id:'own-pr-word',text:'Open PR for saved work',state:'placed',revision:1,createdAt:Date.now(),target:{mode:'none'},attachments:[],reason:null,decision:{kind:'command',command:'open-pr',trackId:null,boundary:null},presentation:{command:'open-pr',operationId:'01a1bdca-b623-72ad-947d-9001299b2260',receiptId:'01a1bdca-b623-72ad-947d-9001299b2261',trackId:null}}];fixture.state.sequence++;fixture.noBase=true;});
    await expect(push.getByRole('button',{name:'Review Push',exact:true})).toBeEnabled();
    await expect.poll(()=>app.evaluate(()=>(globalThis as any).pullFixture.state.utterances[0].state)).toBe('landed');
    await push.getByRole('button',{name:'Review Push',exact:true}).click();await push.getByRole('button',{name:'Continue to PR review',exact:true}).click();
    await expect(sheet.getByText('no base branch yet',{exact:true})).toBeVisible();
    await expect(sheet.getByRole('button',{name:'Prepare PRs',exact:true})).toBeDisabled();
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.creates)).toBe(1);
    await sheet.getByRole('button',{name:'Hide PR review',exact:true}).click();
    await app.evaluate(()=>{const fixture=(globalThis as any).pullFixture;fixture.noBase=false;fixture.state.workerConfigured=true;fixture.state.editorEntries=[{id:881,trackId:null,inputId:null,scene:'commit',title:'Changes committed',line:'Committed. Push when you like.',detail:'A reviewed Commit was saved. Push and PR each need confirmation.',createdAt:Date.now(),expiresAt:Date.now()+12000}];fixture.state.integrations[0].candidate.head='d'.repeat(40);fixture.state.proposal={id:'01a1bdca-b623-52ad-947d-9001299b2280',kind:'publication',label:'Saved work',createdAt:Date.now(),text:'Changes committed · Push and PR?',detail:'Opens saved commits for destination review.',options:[{action:'open-pr',text:'Review Push and PR'},{action:'later',text:'Not now'}],source:{sessionId:'own-dummy-session',selection:{folders:['dummy-web'],through:{'dummy-web':'d'.repeat(40)}}}};fixture.state.sequence++;});
    await expect(page.getByText('Committed. Push when you like.',{exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Review Push and PR',exact:true}).click();
    await expect(push.getByRole('button',{name:'Review Push',exact:true})).toBeEnabled();
    await push.getByRole('button',{name:'Review Push',exact:true}).click();
    const selected=await app.evaluate(()=>(globalThis as any).pullFixture.reviews.at(-1));
    expect(selected.choices).toHaveLength(1);expect(selected.choices[0].folderId).toBe('dummy-web');expect(selected.choices[0].through).toBe('d'.repeat(40));
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.proposals)).toHaveLength(1);
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.pushes)).toHaveLength(0);
    expect(await app.evaluate(()=>(globalThis as any).pullFixture.creates)).toBe(1);
    await closeWindow(app);
  }finally{await app.close();await rm(directory,{recursive:true,force:true});}
});
