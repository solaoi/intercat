import { mockDrafts } from './support/drafts';
import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';

test('worker and composer actions have visible spacing and distinct enabled focus states', async () => {
  const data=await mkdtemp('/private/tmp/ic-controls-');
  const output=process.env.INTERCAT_CONTROLS_OUTPUT ?? 'research/verify/controls-layout-20261105';
  await mkdir(output,{recursive:true});
  const app=await electron.launch({args:['.','--preview','--user-data-dir='+data]});
  await mockDrafts(app);
  const measurements:unknown[]=[];
  try {
    const page=await app.firstWindow();
    await app.evaluate(({ipcMain})=>{
      const state={connected:true,sequence:1,workerConfigured:false,project:{name:"Layout fixture",root:"/fixture"},
        tracks:[{id:'A',label:'A',title:'Login retry',status:'waiting'}],
        workers:[{trackId:'A',phase:'held',sessionId:'session',activeTurn:null,identity:null,detail:null,approvals:[1,2].map(id=>({id:String(id),requirement:{sourceIndex:0},subject:"Review local operation "+id,reason:{kind:"shell",message:"Review this action"},canAllow:true,canDeny:true})),prompts:[],checkpoint:null,checkpointPending:false}],
        utterances:[{id:'initial',text:'ログイン時の再試行処理を修正して、失敗時の表示とテスト結果まで確認する',state:'landed',revision:1,createdAt:1,decision:{kind:'fork',trackId:'A'}}],reports:[{id:"report",trackId:"A",pending:true}],editor:{phase:'ready'}};
      Object.assign(globalThis,{layoutState:state});
      ipcMain.removeHandler('intercat:status');ipcMain.handle('intercat:status',()=>state);
      ipcMain.removeHandler('intercat:output-flows');ipcMain.handle('intercat:output-flows',()=>(state as typeof state & {outputFlows?:unknown[]}).outputFlows??[]);
      ipcMain.removeHandler('intercat:timeline-window');ipcMain.handle('intercat:timeline-window',()=>({revision:1,reset:true,clips:[],removed:[],marks:[],removedMarks:[]}));
    });
    const start=page.getByRole('button',{name:'Start Worker A',exact:true});
    const stop=page.getByRole('button',{name:'Stop Worker A',exact:true});
    const draft=page.getByRole('textbox',{name:'Request draft'});
    const create=page.getByRole('button',{name:'New track',exact:true});
    const put=page.getByRole('button',{name:'Put',exact:true});
    const attach=page.getByRole('button',{name:'Attach reference file',exact:true});
    for (const width of [1440,1040]) {
      await app.evaluate(({BrowserWindow},width)=>{const window=BrowserWindow.getAllWindows()[0];window.setSize(width,760);window.webContents.setZoomFactor(1);},width);
      await expect(start).toBeVisible();
      await expect(attach.locator('svg')).toBeVisible();
      await expect(attach).toHaveText('');
      await expect(page.locator('.reference-files')).toHaveCount(0);
      const attachBox=await attach.boundingBox();
      const actionRow=await page.locator('.composer-bottom').boundingBox();
      expect(attachBox!.x).toBeLessThan(actionRow!.x+45);
      expect(attachBox!.y).toBeGreaterThanOrEqual(actionRow!.y);
      expect(attachBox!.y+attachBox!.height).toBeLessThanOrEqual(actionRow!.y+actionRow!.height);
      const context=page.locator('.desk-selection');
      await expect(context).toHaveText('ログイン時の再試行処理を修正して、失敗時の表示とテスト結果まで確認する');
      expect(await context.getAttribute('title')).toBe(await context.textContent());
      const contextFits=await context.evaluate(element=>element.scrollWidth<=element.clientWidth);
      expect(contextFits).toBe(true);
      await expect(page.getByText('Words become a plan you can steer.',{exact:true})).toHaveCount(0);
      await expect(page.locator('.request-strip')).toContainText('Nothing queued');
      await draft.fill('');await draft.focus();
      await expect(create).toBeDisabled();await expect(put).toBeDisabled();
      const disabled=await put.evaluate(element=>({color:getComputedStyle(element).color,opacity:getComputedStyle(element).opacity}));
      await draft.fill('Apply one change');
      await expect(create).toBeEnabled();await expect(put).toBeEnabled();
      await put.focus();
      const enabled=await put.evaluate(element=>({color:getComputedStyle(element).color,opacity:getComputedStyle(element).opacity,outline:getComputedStyle(element).outlineStyle}));
      const boxes=await Promise.all([start,stop,create,put].map(button=>button.boundingBox()));
      const workerGap=boxes[1]!.x-(boxes[0]!.x+boxes[0]!.width);
      const composerGap=boxes[3]!.x-(boxes[2]!.x+boxes[2]!.width);
      // Card capacity depends on the available height. Open the folded card
      // before measuring the gap between both cards.
      const more=page.getByRole('button',{name:/^\+\d+ more/});
      if(await more.isVisible())await more.click();
      await expect.poll(()=>page.locator('.attention-card:visible').count()).toBeGreaterThanOrEqual(2);
      const cards=await page.locator('.attention-card:visible').all();
      const cardBoxes=await Promise.all(cards.map(card=>card.boundingBox()));
      const cardGaps=cardBoxes.slice(1).map((box,index)=>box!.y-(cardBoxes[index]!.y+cardBoxes[index]!.height));
      for(const gap of cardGaps){expect(gap).toBeGreaterThanOrEqual(6);expect(gap).toBeLessThanOrEqual(12);}
      await expect(put).toBeInViewport();
      for (const gap of [workerGap,composerGap]) {expect(gap).toBeGreaterThanOrEqual(4);expect(gap).toBeLessThanOrEqual(12);}
      expect(enabled.color).not.toBe(disabled.color);
      expect(enabled.opacity).toBe('1');expect(enabled.outline).toBe('solid');
      measurements.push({width,workerGap,composerGap,cardGaps,disabled,enabled});
      await page.screenshot({path:output+'/controls-'+width+'.png'});
      await start.scrollIntoViewIfNeeded();
      await expect(start).toBeInViewport();await expect(put).toBeInViewport();
      await page.screenshot({path:output+'/controls-details-'+width+'.png'});
    }
    await app.evaluate(()=>{
      const state=(globalThis as typeof globalThis & {layoutState:{sequence:number;utterances:{text:string}[]}}).layoutState;
      state.utterances[0].text=state.utterances[0].text.repeat(6);state.sequence++;
    });
    const context=page.locator('.desk-selection');
    await expect.poll(async()=>context.evaluate(element=>element.scrollWidth>element.clientWidth)).toBe(true);
    expect(await context.getAttribute('title')).toBe(await context.textContent());
    expect(await context.evaluate(element=>getComputedStyle(element).textOverflow)).toBe('ellipsis');
    await page.screenshot({path:output+'/desk-long-1040.png'});
    await draft.focus();
    await page.mouse.move(5,5);
    await app.evaluate(()=>{
      const state=(globalThis as typeof globalThis & {layoutState:{sequence:number;workers:{phase:string;detail:string|null}[]}}).layoutState;
      state.workers[0].phase='lost';state.workers[0].detail='The worker lost its connection. Review the saved files before continuing.';
      Object.assign(state,{editor:{phase:'unavailable',canRetry:true,detail:'The model needs attention'},outputFlows:[{trackId:'A',repairRunId:null,queuedBytes:2048,oldestQueuedMs:2500,lagging:true,interrupted:false}]});
      state.sequence++;
    });
    await expect(page.locator('[data-attention-card="worker-A"]')).toBeAttached();
    const more=page.getByRole('button',{name:/^\+\d+ more/});
    if(await more.isVisible())await more.click();
    await expect(page.getByRole('button',{name:'Stop here',exact:true})).toBeAttached();
    await expect(page.getByRole('button',{name:'Stop this worker',exact:true})).toBeAttached();
    const actionCards=[];
    for(const zoom of [1,2]) {
      await app.evaluate(({BrowserWindow},zoom)=>{const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,900);window.webContents.setZoomFactor(zoom);},zoom);
      const positions=await page.locator('.desk-action-card').evaluateAll(cards=>cards.map(card=>{
        const rect=card.getBoundingClientRect(),style=getComputedStyle(card),actions=card.querySelector('.desk-actions')!.getBoundingClientRect();
        return {gap:rect.right-parseFloat(style.paddingRight)-parseFloat(style.borderRightWidth)-actions.right,inside:actions.left>=rect.left&&actions.right<=rect.right};
      }));
      expect(positions.length).toBeGreaterThanOrEqual(3);
      for(const value of positions) {expect(Math.abs(value.gap)).toBeLessThan(1);expect(value.inside).toBe(true);}
      actionCards.push({zoom,positions});
      await page.locator('.desk-scroll').evaluate(element=>{element.scrollTop=0;});
      await page.screenshot({path:output+'/right-actions-top-'+zoom+'.png'});
      await page.getByRole('button',{name:'Stop this worker',exact:true}).scrollIntoViewIfNeeded();
      if (zoom === 2) {
        await expect(page.locator('.sidebar')).toBeHidden();
        await expect(put).toBeInViewport();
        const stopBounds = (await page.getByRole('button',{name:'Stop this worker',exact:true}).boundingBox())!;
        const scrollBounds = (await page.locator('.desk-scroll').boundingBox())!;
        expect(stopBounds.y).toBeGreaterThanOrEqual(scrollBounds.y);
        expect(stopBounds.y + stopBounds.height).toBeLessThanOrEqual(scrollBounds.y + scrollBounds.height);
      }

      await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      // Native capture can precede the compositor's repaint after a programmatic scroll.
      await page.waitForTimeout(300);
      const pixels = await app.evaluate(async ({BrowserWindow}) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(output+'/right-actions-stop-'+zoom+'.png', Buffer.from(pixels, 'base64'));
    }
    await writeFile(output+'/result.json',JSON.stringify({passed:true,measurements,actionCards},null,2));
  } finally {await closeWindow(app);await rm(data,{recursive:true,force:true});}
});

test('a full-length track title stays within its row at 200 percent', async () => {
  const data = await mkdtemp('/private/tmp/ic-title-layout-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  try {
    const page = await app.firstWindow();
    const title = '利用者認証と変更履歴を確認する長い作業名'.repeat(5).slice(0, 80);
    await app.evaluate(({ ipcMain, BrowserWindow }, title) => {
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({
        connected: true, sequence: 1, workerConfigured: false,
        tracks: [{ id: 'A', label: 'A', title, status: 'waiting' }],
        workers: [], utterances: [],
      }));
      ipcMain.removeHandler('intercat:timeline-window');
      ipcMain.handle('intercat:timeline-window', () => ({ revision: 1, reset: true, clips: [], marks: [], removed: [], removedMarks: [] }));
      const window = BrowserWindow.getAllWindows()[0];
      window.setSize(1040, 760);
      window.webContents.setZoomFactor(2);
    }, title);
    const name = page.locator('.track-name');
    await expect(name).toContainText(title);
    await expect(name).toBeVisible();
    expect(await name.evaluate(element => getComputedStyle(element).textOverflow)).toBe('ellipsis');
    expect(await name.evaluate(element => element.scrollWidth > element.clientWidth)).toBe(true);
    const row = await page.getByRole('row').boundingBox();
    const bounds = await name.boundingBox();
    expect(bounds!.height).toBeLessThanOrEqual(row!.height);
    await name.scrollIntoViewIfNeeded();
    await expect(name).toBeInViewport();
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const titlePixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('track-title-200.png'), Buffer.from(titlePixels, 'base64'));
    await page.getByRole('textbox', { name: 'Request draft', exact: true }).fill('Continue this task');
    await expect(page.getByRole('button', { name: 'Put', exact: true })).toBeInViewport();
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
    await writeFile(test.info().outputPath('track-title-composer-200.png'), Buffer.from(pixels, 'base64'));
  } finally {
    await closeWindow(app);
    await rm(data, { recursive: true, force: true });
  }
});

test('the New session plus stays centered and does not shrink at either zoom', async () => {
  const data = await mkdtemp('/private/tmp/ic-sidebar-plus-');
  const app = await electron.launch({ args: ['.', '--preview', '--user-data-dir=' + data] });
  await mockDrafts(app);
  try {
    const page = await app.firstWindow();
    await app.evaluate(({ ipcMain }) => {
      ipcMain.removeHandler('intercat:status');
      ipcMain.handle('intercat:status', () => ({
        connected: true, sequence: 1, workerConfigured: true,
        editor: { phase: 'ready' }, tracks: [], workers: [], utterances: [],
      }));
    });
    for (const zoom of [1, 2]) {
      await app.evaluate(({ BrowserWindow }, zoom) => {
        const window = BrowserWindow.getAllWindows()[0];
        window.setSize(1040, 760);
        window.webContents.setZoomFactor(zoom);
      }, zoom);
      const button = page.getByRole('button', { name: 'New session', exact: true });
      await expect(button).toBeVisible();
      const icon = page.locator('.new-session-icon');
      const wrapper = (await icon.boundingBox())!;
      const drawing = (await icon.locator('svg').boundingBox())!;
      expect(wrapper.width).toBe(20);
      expect(wrapper.height).toBe(20);
      expect(drawing.width).toBe(14);
      expect(drawing.height).toBe(14);
      expect(Math.abs(drawing.x + drawing.width / 2 - wrapper.x - wrapper.width / 2)).toBeLessThan(0.5);
      expect(Math.abs(drawing.y + drawing.height / 2 - wrapper.y - wrapper.height / 2)).toBeLessThan(0.5);
      await page.waitForTimeout(300);
      const pixels = await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64'));
      await writeFile(test.info().outputPath(`sidebar-plus-${zoom}.png`), Buffer.from(pixels, 'base64'));
    }
  } finally {
    await closeWindow(app);
    await rm(data, { recursive: true, force: true });
  }
});
