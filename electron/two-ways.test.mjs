import test from 'node:test';
import assert from 'node:assert/strict';
import { validateTwoWays, validateTwoWaysEdit, finishTwoWays } from './two-ways.mjs';
import { synchronizeFingerprints } from './secrets.mjs';
const id='01960000-0000-7000-8000-000000000011',opId='01960000-0000-7000-8000-000000000012';
const selection={trackId:'own-track',sessionId:'own-session',variation:{modelId:'own-model',providerId:'own-provider',primary:'minimal',alternate:'xhigh'}};
const proof={sessions:[],projects:[]},seal=n=>({revision:n,digest:String(n).repeat(64)});

test('public paired controls accept only exact saved identities and user actions',()=>{
  const read={id},command={id,opId,revision:2,action:'cancel'};
  assert.equal(validateTwoWays('two-ways.read',read),read);
  for(const action of ['cancel','retry','confirm-usage'])assert.equal(validateTwoWays('two-ways.command',{...command,action}).action,action);
  for(const value of [null,[],{}, {...read,id:'wrong'}, {...read,payload:{} }])assert.throws(()=>validateTwoWays('two-ways.read',value));
  for(const value of [{...command,action:'publish'}, {...command,action:'cancelled'}, {...command,id:'wrong'}, {...command,opId:'wrong'}, {...command,revision:0}, {...command,revision:NaN}, {...command,revision:1.5}, {...command,revision:Number.MAX_SAFE_INTEGER+1}, {...command,owner:'renderer'}])assert.throws(()=>validateTwoWays('two-ways.command',value));
  assert.throws(()=>validateTwoWays('two-ways.prepare',command));
  const edit={kind:'end',token:id,revision:2,edit:{kind:'tryTwoWays',opId,selection}};
  assert.equal(validateTwoWaysEdit(edit),edit);
  for(const value of [{...edit,kind:'apply'}, {...edit,owner:id}, {...edit,revision:0}, {...edit,edit:{kind:'tryTwoWays',opId,trackId:'B'}}, {...edit,edit:{kind:'publish',opId}}])assert.throws(()=>validateTwoWaysEdit(value));
});

test('paired receipt confirmation is serialized, replayable after lost acknowledgement, and never forks again',async()=>{
  const before=seal(1),after=seal(2);let receipt={catalog:before,proof},catalog={current:before,sealed:before,proof},fail=false;const calls=[];
  const client={async request(method){calls.push(method);if(method==='secret.catalog')return catalog;if(method==='secret.confirm'){if(fail){fail=false;throw new Error('lost confirmation');}catalog.sealed=receipt.catalog;return {};}if(method==='two-ways.read')return {request:{opId:id},phase:'awaiting-receipt'};throw new Error(method);}};
  let depth=0;
  const store={async read(){assert.equal(depth,1);return receipt;},async publish(expected,next){assert.equal(depth,1);assert.deepEqual(expected,receipt);calls.push('publish');receipt=next;},async run(fn){assert.equal(depth,0);depth++;try{return await fn();}finally{depth--;}}};
  await synchronizeFingerprints(client,store);calls.length=0;catalog={current:after,sealed:before,proof};fail=true;
  await assert.rejects(finishTwoWays(client,{id}),/lost confirmation/);
  assert.deepEqual(calls,['two-ways.read','secret.catalog','publish','secret.confirm']);
  calls.length=0;await finishTwoWays(client,{id});
  assert.deepEqual(calls,['two-ways.read','secret.catalog','publish','secret.confirm','two-ways.read']);assert.deepEqual(catalog.sealed,after);
  calls.length=0;catalog={current:before,sealed:before,proof};
  await assert.rejects(finishTwoWays(client,{id}),{code:'FINGERPRINT_RECEIPT_MISMATCH'});
  assert.deepEqual(calls,['two-ways.read','secret.catalog']);
});

test('paired confirmation rejects a wrong phase or an unverified connection before any receipt update',async()=>{
  const calls=[];let phase='held';const zero=seal(0);
  const client={async request(method){calls.push(method);if(method==='two-ways.read')return {request:{opId:id},phase};if(method==='secret.catalog')return {current:zero,sealed:zero,proof};return {};}};
  await assert.rejects(finishTwoWays(client,{id}),/have not been verified/);assert.deepEqual(calls,[]);
  const receipt={catalog:zero,proof};const store={async read(){return receipt;},async publish(){calls.push('publish');},run(fn){return fn();}};
  await synchronizeFingerprints(client,store);calls.length=0;
  await assert.rejects(finishTwoWays(client,{id}),/not waiting for confirmation/);assert.deepEqual(calls,['two-ways.read']);
});


test('a mismatched paired read cannot publish a fingerprint confirmation',async()=>{
  const calls=[],current=seal(0),receipt={catalog:current,proof};let mismatch=false;
  const client={async request(method){calls.push(method);if(method==='secret.catalog')return {current,sealed:current,proof};if(method==='two-ways.read')return {request:{opId:mismatch?opId:id},phase:'awaiting-receipt'};return {};}};
  const store={async read(){return receipt;},async publish(){calls.push('publish');},run(fn){return fn();}};
  await synchronizeFingerprints(client,store);calls.length=0;mismatch=true;
  await assert.rejects(finishTwoWays(client,{id}),/another request/);assert.deepEqual(calls,['two-ways.read']);
});

test('cancellation seals inherited records and completed confirmations replay without another catalog change',async()=>{
  const calls=[],current=seal(1);let phase='cancelling',receipt={catalog:current,proof};
  const client={async request(method){calls.push(method);if(method==='secret.catalog')return {current,sealed:current,proof};if(method==='two-ways.read')return {request:{opId:id},phase};return {};}};
  const store={async read(){return receipt;},async publish(expected,next){assert.deepEqual(expected,receipt);receipt=next;calls.push('publish');},run(fn){return fn();}};
  await synchronizeFingerprints(client,store);calls.length=0;
  await finishTwoWays(client,{id});assert.deepEqual(calls,['two-ways.read','secret.catalog','publish','secret.confirm','two-ways.read']);
  for(const terminal of ['queued','cancelled']){phase=terminal;calls.length=0;assert.equal((await finishTwoWays(client,{id})).phase,terminal);assert.deepEqual(calls,['two-ways.read']);}
});


test('option queries and the reviewed variation have exact public shapes',()=>{
  const query={inputId:id,revision:3};assert.equal(validateTwoWays('two-ways.options',query),query);
  for(const value of [{...query,inputId:'wrong'},{...query,revision:0},{...query,model:'unreviewed'},null])assert.throws(()=>validateTwoWays('two-ways.options',value));
  const edit={kind:'end',token:id,revision:3,edit:{kind:'tryTwoWays',opId,selection}};
  for(const value of [null,{...selection,workspace:'/private/tmp/unreviewed'},{...selection,variation:{...selection.variation,primary:'high'}},{...selection,variation:{...selection.variation,alternate:'minimal'}},{...selection,variation:{...selection.variation,alternate:'mystery'}},{...selection,variation:{...selection.variation,modelId:''}}])assert.throws(()=>validateTwoWaysEdit({...edit,edit:{...edit.edit,selection:value}}));
});
