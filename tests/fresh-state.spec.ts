import { test, expect } from '@playwright/test';
import type { DaemonStatus } from '../src/bridge';
import { currentWriter, writingTarget, type ConversationContinuation, type FreshJob } from '../src/fresh/types';
import { liveTimeline } from '../src/timeline/state';
import { mergeStatus } from '../src/status';
const link=(source:string,successor:string):ConversationContinuation=>({operation:'dummy-operation',source,successor,sourceSession:source+'-session',sessionId:successor+'-session',boardSession:'dummy-board',origin:{turnNumber:2,endedAt:null,timelineAt:null},ready:true,opened:true});

test('new work follows successive continuations without changing editor choice or unrelated locks',()=>{
  const links=[link('A','A2'),link('A2','A3')];
  const target={mode:'lock' as const,trackId:'A'};
  expect(writingTarget(target,links)).toEqual({mode:'lock',trackId:'A3'});
  expect(target.trackId).toBe('A');
  expect(writingTarget({mode:'lock',trackId:'B'},links)).toEqual({mode:'lock',trackId:'B'});
  expect(writingTarget({mode:'none'},links)).toEqual({mode:'none'});
  expect(currentWriter('A',[...links,link('A3','A')])).toBeNull();
});
test('continued lanes stay below their parent, retain saved context and do not hide old output',()=>{
  const state:DaemonStatus={connected:true,tracks:['A','B','A2','A3'].map(id=>({id,label:id,title:'Generated '+id,status:id==='A'?'done':'waiting'})),continuations:[link('A','A2'),link('A2','A3')]};
  const tracks=liveTimeline(state,[]);
  expect(tracks.map(track=>track.id)).toEqual(['A','A2','A3','B']);
  expect(tracks[0].status).toBe('done');
  expect(tracks[1].branchContext).toBe('context from A’s turn 2 · summarized');
  expect(tracks[2].branchContext).toBe('context from A2’s turn 2 · summarized');
});
test('late status cannot reverse a cancellation acknowledgement; retired history can still disappear',()=>{
  const job:FreshJob={request:{opId:'dummy-id',trackId:'A',sessionId:'dummy-session'},revision:9,phase:'cancelling',successorTrack:'A2',successorSession:'next',checkpoint:null,inspection:null,files:null,summaryReady:false,error:null};
  const state:DaemonStatus={connected:true,daemonPid:17,sequence:100,freshTracks:[job]};
  const old={...state,freshTracks:[{...job,revision:8,phase:'summarizing' as const}]};
  expect(mergeStatus(state,old).freshTracks?.[0]).toBe(job);
  expect(mergeStatus(state,{...old,daemonPid:18}).freshTracks?.[0].phase).toBe('summarizing');
  expect(mergeStatus(state,{...state,freshTracks:[]}).freshTracks).toEqual([]);
});
