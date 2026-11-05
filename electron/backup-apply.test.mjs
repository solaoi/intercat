import test from 'node:test';
import assert from 'node:assert/strict';
import { publishBackupRecovery } from './fingerprint-recovery.mjs';

const source={catalog:{revision:4,digest:'a'.repeat(64)},proof:{sessions:[],projects:[]}};
const target={catalog:{revision:5,digest:'b'.repeat(64)},proof:{sessions:[],projects:[]}};
const request={opId:'01a1a200-0000-7000-8000-000000000011',acknowledgeUnverified:false};
const prepared=phase=>({historyPaused:phase!=='complete',apply:{opId:request.opId,phase},publication:{source,target}});
function fixture(failure) {
  let receipt=structuredClone(source);const calls=[];let failed=false;let complete=false;
  const store={run:run=>run(),read:async()=>receipt,publish:async(expected,next)=>{
    calls.push('publish');assert.ok(JSON.stringify(receipt)===JSON.stringify(expected)||JSON.stringify(receipt)===JSON.stringify(next));receipt=structuredClone(next);
    if(failure==='publication'&&!failed){failed=true;throw new Error('publication acknowledgement lost');}
  }};
  const invoke=async(value,action)=>{
    calls.push(action.action);
    if(action.action==='apply')return prepared(complete?'complete':'awaiting-receipt');
    assert.deepEqual(value,target);assert.deepEqual(receipt,target);complete=true;
    if(failure==='confirmation'&&!failed){failed=true;throw new Error('confirmation acknowledgement lost');}
    return prepared('complete');
  };
  return {store,invoke,calls,get receipt(){return receipt;}};
}
test('backup receipt is published before recovery confirmation',async()=>{
  const f=fixture();assert.deepEqual(await publishBackupRecovery(f.store,request,f.invoke),{applied:true});
  assert.deepEqual(f.calls,['apply','publish','confirmApply']);assert.deepEqual(f.receipt,target);
});
test('lost publication and confirmation acknowledgements resume the same recovery',async()=>{
  for(const phase of ['publication','confirmation']){
    const f=fixture(phase);await assert.rejects(publishBackupRecovery(f.store,request,f.invoke),/acknowledgement lost/);
    assert.deepEqual(f.receipt,target);assert.deepEqual(await publishBackupRecovery(f.store,request,f.invoke),{applied:true});
    assert.equal(f.calls.filter(c=>c==='apply').length,2);
  }
});
test('wrong recovery identities and unrelated receipts are rejected before publication',async()=>{
  for(const changed of [{...prepared('awaiting-receipt'),apply:{opId:'wrong',phase:'awaiting-receipt'}},{...prepared('awaiting-receipt'),publication:{source:{...source,catalog:{...source.catalog,digest:'c'.repeat(64)}},target}}]) {
    const f=fixture();await assert.rejects(publishBackupRecovery(f.store,request,async()=>changed));assert.deepEqual(f.calls,[]);assert.deepEqual(f.receipt,source);
  }
});
