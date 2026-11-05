import { _electron as electron, expect, test } from '@playwright/test';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { closeWindow } from './support/close';

test('Quit offers background work and Stop everything waits for the daemon and process audit', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN!=='1','Opt-in native Keychain and daemon shutdown');
  test.setTimeout(120000);
  const data=await mkdtemp('/private/tmp/ic-quit-');
  const output='research/verify/quit-20261105';
  await mkdir(output,{recursive:true});
  let app=await electron.launch({args:['.','--user-data-dir='+data]});
  let pid:number|undefined;
  const dialogs:unknown[]=[];
  try {
    for (const choice of [0,1]) {
      app.on('console',message=>{
        if(message.text().startsWith('QUIT_FIXTURE:')) dialogs.push(JSON.parse(message.text().slice('QUIT_FIXTURE:'.length)));
      });
      const page=await app.firstWindow();
      await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:30000});
      pid=(await page.evaluate(()=>window.intercat.getStatus())).daemonPid;
      expect(pid).toBeGreaterThan(0);
      await app.evaluate(({dialog},choice)=>{
        dialog.showMessageBox=async (value: import('electron').BaseWindow | import('electron').MessageBoxOptions, options?: import('electron').MessageBoxOptions)=>{
          const shown='message' in value ? value : options!;
          console.log('QUIT_FIXTURE:'+JSON.stringify(shown));
          return {response:shown.message==='Quit Intercat?' ? choice : 2,checkboxChecked:false};
        };
      },choice);
      const closed=app.waitForEvent('close');
      await app.evaluate(({app})=>{app.quit();});
      await closed;
      if (choice===0) {
        process.kill(pid!,0);
        dialogs.push({choice:'background',daemonKept:true,pid});
        app=await electron.launch({args:['.','--user-data-dir='+data]});
      } else {
        await expect.poll(()=>{try{process.kill(pid!,0);return false;}catch{return true;}}).toBe(true);
        dialogs.push({choice:'stop',daemonStopped:true,pid});
        pid=undefined;
      }
    }
    const prompts=dialogs.filter((value):value is {message:string;buttons:string[];defaultId:number}=>typeof value==='object' && value!==null && 'message' in value);
    expect(prompts).toHaveLength(2);
    expect(prompts.every(prompt=>prompt.message==='Quit Intercat?' && prompt.defaultId===0 && prompt.buttons[1]==='Stop everything')).toBe(true);
    await writeFile(output+'/result.json',JSON.stringify({passed:true,dialogs,paidTurns:0},null,2));
  } finally {
    await closeWindow(app);
    if(pid){try{process.kill(pid,'SIGTERM');}catch{}}
    await rm(data,{recursive:true,force:true});
  }
});
