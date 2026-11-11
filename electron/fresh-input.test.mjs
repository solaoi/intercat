import test from 'node:test';
import assert from 'node:assert/strict';
import { validateFresh } from './fresh-input.mjs';
const id='01960000-0000-7000-8000-000000000001';
const track='01960000-0000-7000-8000-000000000002';
const session='01234567-89ab-5cde-8fab-0123456789ab';
test('fresh transport accepts deterministic session identities and preserves exact operations',()=>{
  for(const [method,value] of [
    ['worker.fresh.prepare',{opId:id,trackId:track,sessionId:session}],
    ['worker.fresh.read',{id}],
    ...[{kind:'cancel'},{kind:'retry'},{kind:'allow-access',fingerprint:'a'.repeat(64)}].map(action=>['worker.fresh.command',{opId:track,id,revision:2,action}])
  ])assert.equal(validateFresh(method,value),value);
});
test('fresh transport rejects private lifecycle actions, hidden fields, unsafe revisions and non-operation ids',()=>{
  const command={opId:track,id,revision:2,action:{kind:'cancel'}};
  for(const value of [
    {...command,action:{kind:'cancelled'}},{...command,action:{kind:'transfer'}},
    {...command,action:{kind:'cancel',summary:'private'}},{...command,workspace:'/private/tmp/dummy'},
    {...command,revision:0},{...command,revision:1.5},{...command,revision:Number.MAX_SAFE_INTEGER+1},
    {...command,opId:session},{...command,id:session},
    {...command,action:{kind:'allow-access',fingerprint:'x'.repeat(64)}}
  ])assert.throws(()=>validateFresh('worker.fresh.command',value));
  assert.throws(()=>validateFresh('worker.fresh.prepare',{opId:id,trackId:track,sessionId:session,boundary:{}}));
  assert.throws(()=>validateFresh('worker.fresh.read',{id,summary:true}));
});
