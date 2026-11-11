import {test} from 'node:test';
import assert from 'node:assert/strict';
import {validateComparison} from './comparison-input.mjs';
const id='01960000-0000-7000-8000-000000000001';
const parent={trackId:'01960000-0000-5000-8000-000000000002',sha:'a'.repeat(40)};
const child={trackId:'01960000-0000-5000-8000-000000000003',sha:'b'.repeat(40)};
test('comparison accepts saved branch identities and explicit controls',()=>{
 for(const [method,value] of [['comparison.open',{opId:id,parent,child}],['comparison.control',{opId:id,id,revision:2,attempt:1,action:'hold'}],['comparison.control',{opId:id,id,revision:3,attempt:1,action:'retry'}],['comparison.read',{id}],['comparison.evidence',{id}],['comparison.diff',{id,basis:'paths',path:'tests/own file.spec.ts'}]])assert.equal(validateComparison(method,value),value);
});
test('comparison refuses renderer paths, commands, arbitrary revisions and stale diff shape',()=>{
 for(const [method,value] of [['comparison.open',{opId:id,parent:{...parent,path:'/tmp/own'},child}],['comparison.open',{opId:id,parent,child:parent}],['comparison.control',{opId:id,id,revision:-1,attempt:1,action:'retry'}],['comparison.control',{opId:id,id,revision:2,attempt:1,action:'run',command:'own'}],['comparison.control',{opId:id,id,revision:2,action:'hold'}],['comparison.control',{opId:id,id,revision:2,attempt:0,action:'hold'}],['comparison.control',{opId:id,id,revision:2,attempt:1.5,action:'hold'}],['comparison.diff',{id,side:1,path:'own'}],['comparison.diff',{id,basis:'paths',path:'../own'}],['comparison.diff',{id,basis:'paths',path:'/own'}],['comparison.read',{id,settings:{}}]])assert.throws(()=>validateComparison(method,value));
});
