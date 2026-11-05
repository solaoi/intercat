import { readFile, writeFile } from 'node:fs/promises';
import { Connection, Session } from '../research/verify/support/node_modules/@muse-code/sdk/dist/src/index.js';

const fixture = JSON.parse(await readFile(process.argv[2], 'utf8'));
const results = [];
function projection(session) {
  const f = session.fold;
  return { items:f.items.list(), turns:f.turns(), activeTurnId:f.activeTurnId??null,
    sessionState:Object.fromEntries(f.sessionState.families().map(k=>[k,f.sessionState.get(k)])),
    pendingApprovals:f.pendingApprovals(), resolvedApprovals:f.resolvedApprovals(),
    pendingUserInputs:f.pendingUserInputs(), settledUserInputs:f.settledUserInputs() };
}
for (const test of fixture.cases) {
  const queue=[], waiters=[], sent=[], pending=[], errors=[];
  let ended=false, injected=false;
  const push = frame => { const v={done:false,value:JSON.stringify(frame)+'\n'};
    if(waiters.length) waiters.shift()(v); else queue.push(v); };
  const transport={incoming:{[Symbol.asyncIterator](){return this;},next(){
    if(queue.length)return Promise.resolve(queue.shift());
    if(ended)return Promise.resolve({done:true});
    return new Promise(resolve=>waiters.push(resolve));}},
    async write(line){
      const frame=JSON.parse(line);sent.push(frame);
      if(frame.method!=='view/page')throw new Error(`Unexpected method ${frame.method}`);
      if(!injected){injected=true;for(const event of test.during)push(event);}
      const cursor=frame.params.cursor;
      let events=test.history.slice(test.history.findIndex(e=>e.params.viewCursor===cursor)+1);
      events=events.slice(0,test.pageSize);
      let nextCursor=events.at(-1)?.params.viewCursor??null;
      if(!events.length||nextCursor===test.history.at(-1).params.viewCursor)nextCursor=null;
      if(cursor===test.faultCursor){
        if(test.fault==='error'){push({jsonrpc:'2.0',id:frame.id,error:{code:-32000,message:'fixture failure'}});return;}
        if(test.fault==='stall'){events=[];nextCursor=cursor;}
        if(test.fault==='foreign'){events=structuredClone(events);events[0].params.sessionId='foreign';}
      }
      push({jsonrpc:'2.0',id:frame.id,result:{events,nextCursor}});
    },close(){ended=true;for(const r of waiters.splice(0))r({done:true});}};
  const connection=new Connection(transport);
  const session=new Session({sessionId:test.sessionId,durability:{kind:'durable'},connection});
  session.onGapError(error=>errors.push(error.reason));
  connection.onNotification(frame=>pending.push(session.apply(frame).io));
  connection.onProtocolError(error=>{throw error;});
  for(const frame of test.initial)await session.apply(frame).io;
  await session.apply(test.gap).io;
  await Promise.all(pending);
  for(const frame of test.tail)await session.apply(frame).io;
  const final=projection(session);
  results.push({id:test.id,final,current:session.fold.current,pendingGap:session.fold.pendingGap??null,errors,sent});
  await connection.close();
}
await writeFile(process.argv[3],JSON.stringify({cases:results})+'\n');
console.log(JSON.stringify({cases:results.length}));
