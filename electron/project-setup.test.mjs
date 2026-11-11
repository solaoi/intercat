import test from 'node:test';
import assert from 'node:assert/strict';
import {ProjectSetup} from './project-setup.mjs';

function fixture(inspect) {
  let sequence=0;
  const configurations=[];
  const client={async request(method,params) {
    if(method==='project.selection')return inspect(params.paths);
    if(method==='system.status')return {};
    assert.equal(method,'project.configure');configurations.push(structuredClone(params));
    return {id:params.opId,name:params.name,folders:params.paths};
  }};
  return {client,configurations,setup:new ProjectSetup(async()=>client,()=>`dummy-operation-${++sequence}`)};
}

test('only the picking window can use the resolved roots, and a lost reply reuses its operation',async()=>{
  const f=fixture(paths=>paths.length===3?{kind:'duplicate',roots:['/dummy/root','/dummy/other']}:null);
  const first=await f.setup.begin(1,['/dummy/root','/dummy/root/sub','/dummy/other']);
  assert.equal(first.selection.issue.kind,'duplicate');assert.equal(f.configurations.length,0);
  await assert.rejects(f.setup.resolve(2,{id:first.selection.id,action:'useRoot'}),/no longer available/);
  await assert.rejects(f.setup.resolve(1,{id:first.selection.id,action:'useRoot',paths:['/arbitrary']}),/Invalid/);
  const original=f.client.request.bind(f.client);let lose=true;
  f.client.request=async(method,params)=>{const result=await original(method,params);if(method==='project.configure'&&lose){lose=false;throw new Error('Dummy lost reply');}return result;};
  const lost=await f.setup.resolve(1,{id:first.selection.id,action:'useRoot'});
  assert.equal(lost.selection.mayHaveSaved,true);assert.equal(lost.selection.error,'Dummy lost reply');
  await assert.rejects(f.setup.resolve(1,{id:first.selection.id,action:'cancel'}),/Retry/);
  const saved=await f.setup.resolve(1,{id:first.selection.id,action:'useRoot'});
  assert.deepEqual(f.configurations[0],f.configurations[1]);
  assert.deepEqual(f.configurations[0].paths,['/dummy/root','/dummy/other']);
  assert.equal(f.setup.current(1),null);
  assert.deepEqual(await f.setup.resolve(1,{id:first.selection.id,action:'retry'}),saved);
  assert.equal(f.configurations.length,2,'An IPC reply replay must not reconfigure an acknowledged project');
});

test('ignored and submodule selections only offer cancellation and never configure',async()=>{
  for(const issue of [{kind:'ignored',path:'/dummy/root/ignored',root:'/dummy/root'},{kind:'submodules',path:'/dummy/root'}]) {
    const f=fixture(()=>issue);const result=await f.setup.begin(1,['/dummy/root']);
    await assert.rejects(f.setup.resolve(1,{id:result.selection.id,action:'useRoot'}),/no repository root/);
    await assert.rejects(f.setup.resolve(1,{id:result.selection.id,action:'retry'}),/Resolve/);
    assert.deepEqual(await f.setup.resolve(1,{id:result.selection.id,action:'cancel'}),{project:null});
    assert.deepEqual(await f.setup.resolve(1,{id:result.selection.id,action:'cancel'}),{project:null});
    assert.equal(f.configurations.length,0);
  }
});

test('cancelling an outstanding inspection cannot publish its later result',async()=>{
  let finish;const pending=new Promise(resolve=>{finish=resolve;});const f=fixture(()=>pending);
  const beginning=f.setup.begin(1,['/dummy/root']);
  const current=f.setup.current(1);
  await f.setup.resolve(1,{id:current.selection.id,action:'cancel'});
  finish(null);assert.deepEqual(await beginning,{project:null});assert.equal(f.configurations.length,0);
});

test('a definitive configure rejection permits cancellation and reinspects before retry',async()=>{
  const f=fixture(()=>null);const original=f.client.request.bind(f.client);let failed=false;
  f.client.request=async(method,params)=>{if(method==='project.configure'&&!failed){failed=true;throw Object.assign(new Error('Dummy changed repository'),{code:-32004});}return original(method,params);};
  const failedResult=await f.setup.begin(1,['/dummy/root']);
  assert.equal(failedResult.selection.mayHaveSaved,false);
  const result=await f.setup.resolve(1,{id:failedResult.selection.id,action:'retry'});
  assert.equal(result.project.id,failedResult.selection.id);
});
