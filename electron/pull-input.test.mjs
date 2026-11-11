import test from 'node:test';
import assert from 'node:assert/strict';
import {validatePull} from './pull-input.mjs';
const id='01a1bdca-b623-72ad-947d-9001299b225e';
test('PR requests select saved destinations and reject renderer credentials, API hosts and malformed text',()=>{
  const open={opId:id,pushReview:id,choices:[{folderId:'dummy',base:'main',destination:'a'.repeat(64)}]};
  assert.deepEqual(validatePull('pull.open',open),open);
  for(const request of [{...open,token:'dummy'},{...open,origin:'http://localhost'}, {...open,choices:[...open.choices,...open.choices]},{...open,choices:[{...open.choices[0],destination:'https://dummy.invalid'}]}])assert.throws(()=>validatePull('pull.open',request));
  const command={opId:id,action:{action:'edit',id,revision:1,drafts:[{folderId:'dummy',draft:{title:'Own dummy title',body:'Own generated\nbody'}}]}};
  assert.deepEqual(validatePull('pull.command',command),command);
  assert.throws(()=>validatePull('pull.command',{...command,action:{...command.action,drafts:[{folderId:'dummy',draft:{title:'Own\nheader',body:''}}]}}));
  for(const action of ['confirm','retry','reviewAgain','stop','discard'])assert.doesNotThrow(()=>validatePull('pull.command',{opId:id,action:{action,id,revision:1}}));
  assert.throws(()=>validatePull('pull.command',{opId:id,action:{action:'create',id,revision:1,url:'https://dummy.invalid'}}));
});
test('GitHub links use the saved repository and result, and cannot open renderer URLs',async()=>{
  const {githubLink}=await import('./pull-input.mjs');
  const job={members:[{folderId:'dummy',target:{repository:{owner:'own-dummy',name:'repo'},head:'intercat/own#dummy'},result:{number:7,url:'https://github.com/own-dummy/repo/pull/7'}}]};
  assert.equal(githubLink(job,'dummy','pull'),'https://github.com/own-dummy/repo/pull/7');
  assert.equal(githubLink(job,'dummy','branch'),'https://github.com/own-dummy/repo/tree/intercat/own%23dummy');
  job.members[0].result.url='https://github.com.attacker.invalid/own-dummy/repo/pull/7';
  assert.throws(()=>githubLink(job,'dummy','pull'));
  assert.throws(()=>githubLink(job,'not-a-folder','branch'));
  const existing={members:[{folderId:'dummy',target:job.members[0].target,existing:{number:8,url:'https://github.com/own-dummy/repo/pull/8'},result:null}]};
  assert.equal(githubLink(existing,'dummy','pull'),'https://github.com/own-dummy/repo/pull/8');
});
