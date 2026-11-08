import { expect, test } from '@playwright/test';
import { addPlans } from '../src/plans/timeline';
import type { Plan } from '../src/plans/types';
import type { DaemonStatus, PlanDependencyLocation } from '../src/bridge';
import { dependencyLinks, plannedAnchor } from '../src/timeline/futureLayout';
import { geometry, initialView, LABEL_WIDTH, type Track } from '../src/timeline/model';
import { lanes } from '../src/timeline/viewport';
import { liveTimeline } from '../src/timeline/state';

function fixture(): DaemonStatus {
  const plan: Plan={id:'plan',rev:2,state:'draft',sourceId:'source',sourceRevision:1,startMode:'manual',executeAllowed:false,lockedTrack:null,
    generation:{id:'generation',state:'ready',attempt:0,detail:null},basedOn:[],attachments:[],summary:'Dummy chain',assumptions: [], notices:[],seenRevision:null,rewrite:null,
    items:['A','B','C','A','A'].map((track,index)=>({planItemId:`item-${index}`,trackRef:`new:${track}`,folderId:'dummy',order:index,
      title:`Dummy ${track} ${index}`,instruction:'Dummy only',constraints:[],doneWhen:'Done',after:index===1?[{id:'item-0',inherit:false}]:index===2?[{id:'item-1',inherit:false}]:[],state:'planned',skipOrder:[],startRequested:false,utteranceId:null,detail:null}))};
  const planDependencies=Object.fromEntries(plan.items.map(item=>[item.planItemId,{id:item.planItemId,planId:plan.id,itemId:item.planItemId,retryable:false,trackId:`plan/${item.trackRef}`,title:item.title,state:item.state,unclear:false,clipId:null,atMs:null} satisfies PlanDependencyLocation]));
  return {connected:true,plans:[plan],planDependencies};
}

test('every lane begins with its own first item at one and two tile widths',()=>{
  const state=fixture(),tracks=addPlans([],state),rows=lanes(tracks);
  expect(tracks.map(track=>track.planned[0].id)).toEqual(['item-0','item-1','item-2']);
  expect(tracks[2].planned[0].after).toBe('after New B');
  for(const width of [496,788,1150]){
    const first=rows.map(row=>plannedAnchor(row,0,width));
    expect(new Set(first.map(anchor=>anchor!.x)).size).toBe(1);
    expect(first.every(anchor=>!anchor!.grouped)).toBe(true);
    expect(geometry(width).tileWidth).toBe(152);
    expect(dependencyLinks(rows,width,initialView,null)).toEqual([]);
    expect(dependencyLinks(rows,width,initialView,'item-1').map(edge=>edge.id)).toEqual(['item-0/item-1','item-1/item-2']);
  }
});

test('hidden parents connect at their lane aggregate without hiding the child head',()=>{
  const state=fixture();state.plans![0].items[2].after=[{id:'item-4',inherit:false}];
  const rows=lanes(addPlans([],state));
  for(const width of [496,1150]){
    const edge=dependencyLinks(rows,width,initialView,'item-2')[0];
    expect(edge.from).toBe('item-4');expect(edge.points[0].x).toBe(width-20);
    const target=plannedAnchor(rows[2],0,width)!;
    expect(edge.points.at(-1)).toEqual({x:target.x,y:target.y});
  }
});

test('cut and missing dependencies keep the waiting reason and have no fabricated line',()=>{
  const state=fixture();state.planDependencies!['item-1'].state='cut';
  let tracks=addPlans([],state);
  expect(tracks[2].planned[0].after).toContain('was cut');
  expect(dependencyLinks(lanes(tracks),788,initialView,'item-2')).toEqual([]);
  delete state.planDependencies!['item-1'];tracks=addPlans([],state);
  expect(tracks[2].planned[0].after).toBe('held · dependency unavailable');
  state.plans![0].state='held';
  state.planDependencies!['item-1']={id:'item-1',planId:null,itemId:null,retryable:false,unavailable:true,trackId:'',title:'Unavailable dependency',state:'failed',unclear:false,clipId:null,atMs:null};
  tracks=addPlans([],state);
  expect(tracks[2].planned[0].after).toBe('held · dependency unavailable');
  expect(dependencyLinks(lanes(tracks),788,initialView,'item-2')).toEqual([]);
});

test('an external saved clip remains a dependency outside the fetched time window',()=>{
  const state=fixture();state.plans![0].items[0].after=[{id:'clip-outside',inherit:false}];
  state.planDependencies!['clip-outside']={id:'clip-outside',planId:null,itemId:null,retryable:false,trackId:'external',title:'External',state:'succeeded',unclear:false,clipId:'clip-outside',atMs:0};
  const external:Track={id:'external',label:'X',title:'External dummy track',folder:'dummy',status:'done',color:0xaaaaaa,clips:[],planned:[],repairs:[]};
  const rows=lanes(addPlans([external],state)),edge=dependencyLinks(rows,788,initialView,'item-0').find(edge=>edge.from==='clip-outside')!;
  expect(edge.points[0]).toEqual({x:LABEL_WIDTH+2,y:53});
  expect(edge.description).toContain('X → Dummy A 0');
});

test('ordinary waiting words stay in their execution order before plan items',()=>{
  const status:DaemonStatus={connected:true,tracks:[{id:'A',label:'A',title:'Dummy',status:'waiting'}],utterances:[2,1].map(index=>({
    id:`word-${index}`,text:`Dummy ${index}`,state:'placed',revision:1,createdAt:index,attachments:[],reason:null,target:{mode:'lock',trackId:'A'},
    decision:{kind:'extend',trackId:'A',boundary:'after-turn',command:null,confidence:1,inferenceMs:0,timedOut:false}
  }))};
  expect(liveTimeline(status,[])[0].planned.map(item=>item.id)).toEqual(['word-1','word-2']);
});

test('a failed earlier item holds later items on the same track without an explicit after edge',()=>{
  const state=fixture();
  state.plans![0].state='running';
  state.plans![0].items[0].state='failed';
  state.planDependencies!['item-0'].state='failed';
  const track=addPlans([],state).find(track=>track.id==='plan/new:A')!;
  const later=track.planned.find(item=>item.id==='item-3')!;
  expect(later.blocked).toBe(true);
  expect(later.after).toContain('item didn’t finish');
  state.plans![0].state='held';
  expect(addPlans([],state).find(track=>track.id==='plan/new:A')!.planned.find(item=>item.id==='item-3')!.after).toContain('item didn’t finish');
});

test('a parent in a later plan keeps its own track label and cut reason',()=>{
  const state=fixture(),parent=structuredClone(state.plans![0]);
  parent.id='parent-plan';parent.items=[{...parent.items[0],planItemId:'parent-item',trackRef:'new:Parent'}];
  state.plans!.push(parent);
  state.plans![0].items[0].after=[{id:'parent-item',inherit:false}];
  state.planDependencies!['parent-item']={id:'parent-item',planId:parent.id,itemId:'parent-item',retryable:false,trackId:'parent-plan/new:Parent',title:'Parent task',state:'planned',unclear:false,clipId:null,atMs:null};
  let tracks=addPlans([],state);
  expect(tracks[0].planned[0].after).toBe('after New Parent');
  expect(dependencyLinks(lanes(tracks),788,initialView,'item-0').find(edge=>edge.from==='parent-item')).toBeDefined();
  parent.items[0].state='cut';state.planDependencies!['parent-item'].state='cut';
  tracks=addPlans([],state);
  expect(tracks[0].planned[0].after).toBe('held · New Parent’s item was cut');
  expect(dependencyLinks(lanes(tracks),788,initialView,'item-0').some(edge=>edge.from==='parent-item')).toBe(false);
});

test('later plans show and can identify failed predecessors on the same real track',async()=>{
  const {blockedDependencies,orderedPredecessors}=await import('../src/plans/dependencies');
  const state=fixture();const first=state.plans![0];first.items=first.items.slice(0,1);first.items[0].trackRef='A';first.items[0].state='failed';
  const later=structuredClone(first);later.id='later-plan';later.items[0].planItemId='later-item';later.items[0].title='Later work';later.items[0].state='planned';
  state.plans!.push(later);state.tracks=[{id:'A',label:'A',title:'Dummy A',status:'done'}];
  state.planDependencies!['item-0']={...state.planDependencies!['item-0'],trackId:'A',state:'failed'};
  state.planDependencies!['later-item']={...state.planDependencies!['item-0'],id:'later-item',planId:later.id,itemId:'later-item',state:'planned'};
  expect(orderedPredecessors(later,later.items[0],state.plans!)).toEqual([first.items[0]]);
  expect(blockedDependencies(later,later.items[0],state.plans!,state.planDependencies!)[0].id).toBe('item-0');
  expect(liveTimeline(state,[]).find(track=>track.id==='A')!.planned.find(item=>item.id==='later-item')!.after).toBe('held · A’s item didn’t finish');
  later.items[0].skipOrder=['item-0'];expect(orderedPredecessors(later,later.items[0],state.plans!)).toEqual([]);
  first.items[0].trackRef='new:1';later.items[0].trackRef='new:1';later.items[0].skipOrder=[];expect(orderedPredecessors(later,later.items[0],state.plans!)).toEqual([]);
});
