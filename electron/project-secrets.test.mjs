import test from 'node:test';
import assert from 'node:assert/strict';
import { projectSecretStore, projectRegistration } from './project-secrets.mjs';
const projectId='a'.repeat(64),op1='01a1a200-0000-7000-8000-000000000001',op2='01a1a200-0000-7000-8000-000000000002';
const check = async () => {};
const change=(store,opId,name,value,expectedRevision=null,minimumRevision=0,validate=check)=>store.change({opId,name,value,expectedRevision,minimumRevision},validate);
function entry(){let value=null;return {getPassword:async()=>value,setPassword:async next=>{value=next;}};}
test('Keychain updates are atomic, sorted and idempotent and deletion retains the revision',async()=>{
 const store=projectSecretStore(projectId,entry());
 assert.deepEqual(await store.load(),{projectId,revision:0,entries:[]});
 const first=await change(store,op1,'TOKEN','dummy-secret');
 assert.equal(first.revision,1);assert.deepEqual(await change(store,op1,'TOKEN','dummy-secret'),first);
 await assert.rejects(change(store,op1,'TOKEN','different'));
 await assert.rejects(change(store,op2,'TOKEN',null,0));
 const deleted=await change(store,op2,'TOKEN',null,1);assert.equal(deleted.revision,2);assert.deepEqual(deleted.entries,[]);
 assert.deepEqual(await change(store,op2,'TOKEN',null,1),deleted);
});
test('project registration sends fingerprints without values and preserves byte lengths',()=>{
 const result=projectRegistration({projectId,revision:1,entries:[{name:'TOKEN',value:'dummy-猫-token'}]},op1,'07'.repeat(32));
 assert.equal(result.fingerprints[0].byteLen,Buffer.byteLength('dummy-猫-token'));
 assert.equal(result.fingerprints[0].tag.length,64);
 assert.equal(JSON.stringify(result).includes('dummy-猫-token'),false);
});
test('invalid or oversized values never reach the Keychain writer',async()=>{
 const store=projectSecretStore(projectId,entry());
 for(const name of ['PATH','HOME','MUSE_MODEL','bad']) await assert.rejects(change(store,op1,name,'dummy'));
 for(const value of ['', 'a\0b', 'x'.repeat(8193)]) await assert.rejects(change(store,op1,'TOKEN',value));
 await assert.rejects(change(store,'not-an-operation','TOKEN','dummy'));
});

test('a rejected preflight never changes Keychain and a later edit remains possible',async()=>{
 const keychain=entry(),store=projectSecretStore(projectId,keychain);
 await change(store,op1,'TOKEN','dummy-old');
 const before=await keychain.getPassword();
 await assert.rejects(change(store,op2,'TOKEN','dummy-new',1,1,async()=>{throw new Error('fingerprint capacity');}),/capacity/);
 assert.equal(await keychain.getPassword(),before);
 assert.deepEqual((await change(store,op2,'TOKEN',null,1,1)).entries,[]);
});
test('a missing or older Keychain bundle advances beyond the confirmed manifest when explicitly edited',async()=>{
 const store=projectSecretStore(projectId,entry());
 const result=await change(store,op1,'TOKEN',null,0,8);
 assert.equal(result.revision,9);assert.deepEqual(result.entries,[]);
});
test('a concurrent saved edit during validation cannot be overwritten',async()=>{
 const keychain=entry(),store=projectSecretStore(projectId,keychain);
 await assert.rejects(change(store,op1,'TOKEN','dummy',0,0,async()=>{
   await keychain.setPassword(JSON.stringify({projectId,revision:1,entries:[{name:'OTHER',value:'newer-dummy'}]}));
 }),/another app/);
 assert.deepEqual((await store.load()).entries,[{name:'OTHER',value:'newer-dummy'}]);
});
test('a write with a lost readback can be retried without a second Keychain mutation',async()=>{
 let saved=null,writes=0,fail=false;
 const keychain={getPassword:async()=>{if(fail){fail=false;throw new Error('lost readback');}return saved;},setPassword:async value=>{saved=value;writes++;fail=true;}};
 const store=projectSecretStore(projectId,keychain);
 await assert.rejects(change(store,op1,'TOKEN','dummy'),/confirm/);
 assert.equal((await change(store,op1,'TOKEN','dummy')).revision,1);
 assert.equal(writes,1);
});
