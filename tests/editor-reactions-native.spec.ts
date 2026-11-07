import { _electron as electron, expect, test } from '@playwright/test';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { configureEditor } from './support/editor';
import { closeWindow } from './support/close';

test('native privacy notices follow screened requests and pasted copies without disclosing values', async () => {
  test.skip(process.env.INTERCAT_TEST_KEYCHAIN !== '1', 'Opt-in isolated daemon and Keychain');
  test.setTimeout(180_000);
  const data=await mkdtemp('/private/tmp/ic-editor-reactions-');
  const model=path.join(data,'model');const python=path.join(data,'python');
  await mkdir(model);
  await writeFile(python,'#!/usr/bin/python3\n'+await readFile('tests/fixtures/park_editor.py','utf8'),{mode:0o700});
  const app=await electron.launch({args:['.','--user-data-dir='+data]});
  let pid:number|undefined;
  try {
    const page=await app.firstWindow();
    await expect(page.locator('.status-bar')).toContainText('Daemon connected',{timeout:95_000});
    pid=(await page.evaluate(()=>window.intercat.getStatus())).daemonPid;
    await configureEditor(app,data,model,python);
    await expect(page.locator('.status-bar')).toContainText('Local editor · ready',{timeout:60_000});
    const draft=page.getByRole('textbox',{name:'Request draft',exact:true});
    await draft.fill('When I say, create dummy arithmetic tests.');
    await page.getByRole('button',{name:'New track',exact:true}).click();
    await expect.poll(async()=>(await page.evaluate(()=>window.intercat.getStatus())).tracks?.length).toBe(1);
    const track=(await page.evaluate(()=>window.intercat.getStatus())).tracks![0].id;
    await draft.fill('park Contact dummy.person@example.invalid after review.');
    await page.getByRole('button',{name:'Put',exact:true}).click();
    const line=page.getByRole('region',{name:'Editor explanation',exact:true});
    await expect(line).toContainText('I masked private details in this request.');
    const logged=await page.evaluate(trackId=>window.intercat.getEditorLog({trackId,before:null}),track);
    const inputNotice=logged.entries.find(entry=>entry.scene==='masked');
    expect(inputNotice?.inputId).toBeTruthy();
    expect(JSON.stringify(inputNotice)).not.toContain('dummy.person@example.invalid');
    await line.getByRole('button',{name:'more ›'}).click();
    const popup=page.getByRole('dialog',{name:'Editor explanation details'});
    await expect(popup).toContainText('before it was passed to the Editor');
    await page.keyboard.press('Escape');
    const pasted=await page.evaluate(async()=>{const opId=await window.intercat.createOperationId();await window.intercat.stagePastedText({opId,text:'Reference contact: second.person@example.invalid'});await window.intercat.finishPastedText(opId);return opId;});
    await expect(line).toContainText('I masked private details in the pasted text.');
    expect(await page.evaluate(id=>window.intercat.readPastedText(id),pasted)).toContain('second.person@example.invalid');
    const after=await page.evaluate(trackId=>window.intercat.getEditorLog({trackId,before:null}),track);
    expect(after.entries.filter(entry=>entry.scene==='masked')).toHaveLength(2);
    expect(JSON.stringify(after)).not.toContain('dummy.person@example.invalid');
    expect(JSON.stringify(after)).not.toContain('second.person@example.invalid');
    await app.evaluate(({BrowserWindow})=>{const window=BrowserWindow.getAllWindows()[0];window.setSize(1040,760);window.webContents.setZoomFactor(2);});
    await line.scrollIntoViewIfNeeded();
    await expect(line.getByRole('button',{name:'more ›'})).toBeInViewport();
    await line.getByRole('button',{name:'more ›'}).click();
    await expect(popup).toContainText('pasted original stays on this Mac');
    await writeFile(test.info().outputPath('native-mask-200.png'),Buffer.from(await app.evaluate(async({BrowserWindow})=>(await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')),'base64'));
    const calls=await readFile(path.join(data,'daemon/model-calls.jsonl'),'utf8');
    expect(calls).not.toContain('dummy.person@example.invalid');
    expect(calls).toContain('<redacted:email#');
    await writeFile(test.info().outputPath('editor-log.json'),JSON.stringify(after,null,2));
  } finally {
    await closeWindow(app);
    if(pid){try{process.kill(pid,'SIGTERM');}catch{}await expect.poll(()=>{try{process.kill(pid!,0);return true;}catch{return false;}},{timeout:15_000}).toBe(false);}
    await rm(data,{recursive:true,force:true});
  }
});
