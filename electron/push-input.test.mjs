import test from 'node:test';
import assert from 'node:assert/strict';
import { validatePush } from './push-input.mjs';
const id='01a1bdca-b623-72ad-947d-9001299b225e';
test('Push requests bind confirmation to a saved review and reject renderer transport settings',()=>{
  const confirm={opId:id,action:{action:'confirm',review:id}};
  assert.deepEqual(validatePush('push.command',confirm),confirm);
  for(const action of [{action:'send',url:'file:///dummy'}, {...confirm.action,url:'https://dummy.invalid'}, {action:'confirm',review:'not-a-review'}, {action:'retry',id,revision:-1}]) assert.throws(()=>validatePush('push.command',{opId:id,action}));
  const request={opId:id,choices:[{folderId:'own-generated-folder',remote:'origin',targetRef:null,through:null}]};
  assert.deepEqual(validatePush('push.review',request),request);
  assert.throws(()=>validatePush('push.review',{...request,choices:[...request.choices,...request.choices]}));
  assert.throws(()=>validatePush('push.review',{...request,choices:[{...request.choices[0],remote:'origin\n--mirror'}]}));
  assert.throws(()=>validatePush('push.review',{...request,choices:[{...request.choices[0],url:'https://dummy.invalid'}]}));
});
