import test from 'node:test';
import assert from 'node:assert/strict';
import { registration, validSecretName, acceptCatalog, synchronizeFingerprints, validReceipt } from './secrets.mjs';

const key='07'.repeat(32);
const request={opId:'01a1a200-0000-7000-8000-000000000001',trackId:'track',sessionId:'session',name:'NPM_TOKEN',value:'fixture-secret-猫-value'};

test('only one-way metadata crosses the daemon boundary and HMAC matches the shared vector',()=>{
  const result=registration(request,key);
  assert.equal(result.byteLen,24);
  assert.equal(result.tag,'5d5a3ea70c546ff4b7cf093accbd9eab884ffdcca3e2f8a8e916c455ed14efe2');
  assert.equal(result.keyCheck,'34aba8963075a6c5ab6d9cdcabb229d3d2f85b2b0b22524de16894c71646e067');
  assert.equal('value' in result,false);
  assert.equal(JSON.stringify(result).includes(request.value),false);
  assert.equal(JSON.stringify(result).includes(key),false);
});

test('reserved environment variables and malformed values never produce registrations',()=>{
  for(const name of ['PATH','HOME','GIT_DIR','INTERCAT_HOST','MUSE_MODEL','XDG_CONFIG_HOME','DYLD_INSERT_LIBRARIES','LD_PRELOAD','SSH_AUTH_SOCK','NODE_OPTIONS','lowercase','BAD-NAME']) {
    assert.equal(validSecretName(name),false,name);
    assert.throws(()=>registration({...request,name},key));
  }
  for(const value of ['', 'value\0suffix','x'.repeat(65537)]) assert.throws(()=>registration({...request,value},key));
  assert.throws(()=>registration({...request,opId:'not-an-id'},key));
  assert.equal(validSecretName('AWS_SECRET_ACCESS_KEY'),true);
});

const proof={sessions:[],projects:[]};
const receipt=catalog=>({catalog,proof});
const catalog=(current,sealed)=>({current,sealed,proof});
const empty={revision:0,digest:'0'.repeat(64)};
const first={revision:1,digest:'1'.repeat(64)};
const second={revision:2,digest:'2'.repeat(64)};
test('receipt witnesses reject duplicate owners, obsolete shapes and altered matching seals',()=>{
  const session={sessionId:'s',trackId:'track',digest:'a'.repeat(64)};
  const saved={catalog:first,proof:{sessions:[session],projects:[]}};
  assert.equal(validReceipt(saved),true);
  assert.equal(validReceipt(first),false);
  assert.equal(validReceipt({...saved,proof:{sessions:[session,session],projects:[]}}),false);
  assert.throws(()=>acceptCatalog({current:first,sealed:first,proof:{sessions:[{...session,trackId:'foreign'}],projects:[]}},saved),{code:'FINGERPRINT_RECEIPT_MISMATCH'});
  const reordered={catalog:first,proof:{projects:[],sessions:[{digest:session.digest,trackId:session.trackId,sessionId:session.sessionId}]}};
  assert.deepEqual(acceptCatalog({current:first,sealed:first,proof:saved.proof},reordered),saved);
});
test('only a new catalog or an interrupted forward seal can be accepted',()=>{
  assert.deepEqual(acceptCatalog(catalog(empty,null),null),receipt(empty));
  assert.deepEqual(acceptCatalog(catalog(first,empty),receipt(empty)),receipt(first));
  assert.deepEqual(acceptCatalog(catalog(first,empty),receipt(first)),receipt(first));
  assert.deepEqual(acceptCatalog(catalog(first,first),receipt(first)),receipt(first));
  for(const [candidate,saved] of [
    [catalog(empty,empty),null],
    [catalog(empty,empty),receipt(first)],
    [catalog(first,empty),receipt(second)],
    [catalog(first,first),receipt(empty)],
  ]) assert.throws(()=>acceptCatalog(candidate,saved),error=>error.code==='FINGERPRINT_RECEIPT_MISMATCH');
  assert.throws(()=>acceptCatalog({current:empty,sealed:{...empty,digest:'f'.repeat(64)}},empty));
});

test('daemon confirmation always follows verified external storage',async()=>{
  const order=[];
  const client={request:async(method,params)=>{order.push(method);if(method==='secret.catalog')return catalog(first,empty);assert.deepEqual(params,first);return{confirmed:true};}};
  const store={run:fn=>fn(),read:async()=>receipt(empty),publish:async(previous,next)=>{assert.deepEqual(previous,receipt(empty));assert.deepEqual(next,receipt(first));order.push('keychain');}};
  await synchronizeFingerprints(client,store);
  assert.deepEqual(order,['secret.catalog','keychain','secret.confirm']);
  order.length=0;
  await assert.rejects(synchronizeFingerprints(client,{...store,publish:async()=>{throw new Error('storage unavailable');}}));
  assert.deepEqual(order,['secret.catalog']);
});
