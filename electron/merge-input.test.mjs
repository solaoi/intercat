import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import path from 'node:path';
import {createHash} from 'node:crypto';
import {applyMergeInput} from './merge-input.mjs';
const request={opId:'01a1a200-0000-7000-8000-000000000001',id:'01a1a200-0000-7000-8000-000000000002',revision:8,files:[{kind:'text',path:'dummy.txt',version:'dummy-version',text:'日本語\n'.repeat(60000)}]};
test('large manual edits send only a private input identifier and bind the digest',async()=>{
  const directory=await fs.mkdtemp('/private/tmp/ic-manual-input-');
  try{
    const result=await applyMergeInput({directory,request,client:async()=>({request:async(method,params)=>{
      assert.equal(method,'merge.apply');assert.deepEqual(Object.keys(params).sort(),['digest','id','input','opId','revision']);
      assert.ok(Buffer.byteLength(JSON.stringify(params))<512);
      const box=path.join(directory,params.input),file=path.join(box,'input.json');
      assert.equal((await fs.stat(box)).mode&0o777,0o700);assert.equal((await fs.stat(file)).mode&0o777,0o600);
      const bytes=await fs.readFile(file);assert.ok(bytes.length>512*1024);assert.equal(createHash('sha256').update(bytes).digest('hex'),params.digest);assert.deepEqual(JSON.parse(bytes),request);
      return {phase:'captured'};
    }})});
    assert.deepEqual(result,{phase:'captured'});assert.deepEqual(await fs.readdir(directory),[]);
    await assert.rejects(applyMergeInput({directory,request,client:async()=>({request:async()=>{throw new Error('dummy lost acknowledgement');}})}),/lost acknowledgement/);
    assert.deepEqual(await fs.readdir(directory),[]);
  }finally{await fs.rm(directory,{recursive:true,force:true});}
});
test('manual input refuses symlink directories and reused invalid identifiers',async()=>{
  const root=await fs.mkdtemp('/private/tmp/ic-manual-input-');
  try{
    const outside=path.join(root,'dummy-outside');await fs.mkdir(outside);await fs.symlink(outside,path.join(root,'link'));
    await assert.rejects(applyMergeInput({directory:path.join(root,'link'),request,client:async()=>{throw new Error('must not call');}}),/Unsafe/);
    await assert.rejects(applyMergeInput({directory:outside,request:{...request,opId:'../../escape'},client:async()=>{throw new Error('must not call');}}),/Invalid/);
    assert.deepEqual(await fs.readdir(outside),[]);
  }finally{await fs.rm(root,{recursive:true,force:true});}
});
