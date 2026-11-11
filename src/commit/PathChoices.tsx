import ComparisonPanel from '../comparison/ComparisonPanel';
import type {Comparison,Selection} from '../comparison/types';
import {useState} from 'react';
import type {Effect,WorkerState} from '../bridge';
import type {IntegrationAction,IntegrationJob} from './types';
import {RealignButton} from './Realignment';
import type {RealignJob} from './realign';
import type {PathFamily} from './paths';

function continuedMember(family:PathFamily,id:string){
  const seen=new Set<string>();
  while(!seen.has(id)){
    seen.add(id);
    const next=family.forks.find(fork=>fork.kind==='continuation'&&fork.parent===id);
    if(!next)return family.members.find(member=>member.id===id);
    id=next.child;
  }
  return undefined;
}

export function PathChoices({job,comparisons,connected,realignments,queued,workers,effects,disabled,execute,showTrack}:{job:IntegrationJob;comparisons:Comparison[];connected:boolean;realignments:RealignJob[];queued:(job:RealignJob)=>void;workers:WorkerState[];effects:Effect[];disabled:boolean;execute:(action:IntegrationAction)=>void;showTrack:(id:string)=>void}) {
  const [comparison,setComparison]=useState<Selection|null>(null);
  const [shown,setShown]=useState<string|null>(null);
  const state=job.pathState;
  const families=job.pathReview.families.filter(family=>family.forks.some(fork=>fork.kind==='branch')&&(state?.pending.includes(family.root)||job.pathReview.choices[family.root]));
  if(!families.length)return null;
  const label=(id:string)=>job.tracks.find(track=>track.id===id)?.label??families.flatMap(family=>family.members).find(member=>member.id===id)?.label??'Unknown path';
  const choose=(track:string)=>execute({action:'choosePath',id:job.id,revision:job.revision,track});
  const undoConflict=job.candidate?.undoConflict??job.plan?.undoConflict;
  const activeRetirements=job.retirements.filter(entry=>entry.job===job.id);
  return <section className="commit-paths" aria-label="Paths to keep">
    <h3>Choose the path to keep</h3>
    {families.map(family=>{
      const choice=job.pathReview.choices[family.root];
      return <fieldset key={family.root} disabled={disabled}><legend>Worker {label(family.root)}’s paths</legend>
        {family.members.filter(member=>!family.forks.some(fork=>fork.kind==='continuation'&&fork.parent===member.id)).map(member=>{
          const retired=choice&&member.id!==choice.kept;
          const running=workers.some(worker=>worker.trackId===member.id&&!!worker.activeTurn);
          const hasEffects=effects.some(effect=>effect.trackId===member.id);
          const aside=job.retirements.some(entry=>entry.track===member.id&&entry.phase==='aside');
          const notice=aside?'Set aside · Restore to choose this path.':!member.checkpoint?'No saved checkpoint yet.':!member.available?'Preparing this path…':!choice?'Choose this path to include it.':retired?`Will be set aside${running?' · running now · stops when you commit':''}`:'This choice is saved in the review.';
          return <label className="commit-path-option" key={member.id}><input type="radio" name={`path-${job.id}-${family.root}`} checked={choice?.kept===member.id} disabled={!member.available} onChange={()=>choose(member.id)}/><span><strong>Keep Worker {member.label}</strong><small>{notice}{retired&&hasEffects?' · effects stay':''}</small></span></label>;
        })}
        <div className="comparison-launch">{family.forks.filter(fork=>fork.kind==='branch').map(fork=>{
          const parent=continuedMember(family,fork.parent),child=continuedMember(family,fork.child);
          const parentTrack=job.tracks.find(track=>track.id===parent?.id),childTrack=job.tracks.find(track=>track.id===child?.id);
          const saved=[...comparisons].reverse().find(value=>value.request.parent.trackId===parent?.id&&value.request.child.trackId===child?.id&&value.request.parent.sha===parent?.checkpoint?.sha&&value.request.child.sha===child?.checkpoint?.sha);
          const available=!!parent?.checkpoint&&!!child?.checkpoint&&!!parentTrack?.checkpointCount&&!!childTrack?.checkpointCount;
          return <button key={fork.child} disabled={disabled||!connected||!available} onClick={()=>{if(parent?.checkpoint&&child?.checkpoint)setComparison({parent:{trackId:parent.id,sha:parent.checkpoint.sha},child:{trackId:child.id,sha:child.checkpoint.sha},...(saved?{jobId:saved.id}:{})});}}>{saved?'Compare saved results':'Check both paths'} · {label(parent?.id??fork.parent)} / {label(child?.id??fork.child)}</button>;
        })}</div>
      </fieldset>;
    })}
    {comparison&&<ComparisonPanel key={JSON.stringify(comparison)} selection={comparison} connected={connected} close={()=>setComparison(null)}/>}
    {!!state?.undoRecords&&<p className="commit-path-undo">Replaces published changes · adds 1 undo commit <small>{state.undoRecords} saved integration record{state.undoRecords===1?'':'s'}</small></p>}
    {state?.mixed.map(mixed=>{
      const key=`${mixed.source}:${mixed.dependency}:${mixed.record}`;
      const extending=mixed.kind==='extendsCancelledContext';
      const explanation=extending
        ?`${label(mixed.track)}’s commit extends ${label(mixed.dependency)}’s changes first published with ${label(mixed.source)}`
        :`${label(mixed.source)}’s later changes are mixed into ${label(mixed.track)}’s commit`;
      return <div className="commit-path-blocker" key={key}>
        <p>{explanation} · can’t replace {label(mixed.source)} automatically</p>
        <div><button disabled={disabled||!families.some(family=>family.members.some(member=>member.id===mixed.source&&member.available))} onClick={()=>choose(mixed.source)}>Keep {label(mixed.source)}</button><button onClick={()=>{setShown(key);showTrack(mixed.track);}}>Show me</button></div>
        <details open={shown===key}><summary onClick={event=>{event.preventDefault();setShown(shown===key?null:key);}}>Published dependency</summary>
          <p>Worker {label(mixed.track)}’s commit added {label(mixed.dependency)}’s saved changes after checkpoint {mixed.after}, through checkpoint {mixed.through}.{extending?` Its starting context was published with ${label(mixed.source)}’s commit and would also be undone.`:''}</p>
        </details>
      </div>;
    })}
    {state?.descendants.map(child=><div className="commit-path-blocker" key={child.track}><p>{label(child.track)} inherits later changes from {child.parents.map(parent=>label(parent.track)).join(', ')}.</p>{child.included?<div>{child.parents.map(parent=><RealignButton key={parent.track} source={{kind:'reparent',jobId:job.id,revision:job.revision,parent:parent.track}} title={`Realign ${label(child.track)} onto ${label(parent.onto)}`} track={{id:child.track,label:label(child.track)}} jobs={realignments} workers={workers} disabled={disabled} queued={queued}/>)}<button disabled={disabled} onClick={()=>execute({action:'select',id:job.id,revision:job.revision,track:child.track,included:false})}>Leave {label(child.track)} out of this commit</button></div>:<small>Left out · its waiting requests will be held when you commit.</small>}</div>)}
    {undoConflict&&<div className="commit-path-blocker" role="status"><p>The undo conflicts with later edits.</p><ul>{undoConflict.paths.map(path=><li key={path}>{path}</li>)}</ul></div>}
    {activeRetirements.some(entry=>['stopping','saving'].includes(entry.phase))&&<p role="status">Stopping the unchosen paths and saving their final checkpoints…</p>}
    {activeRetirements.filter(entry=>entry.error).map(entry=><p className="commit-error" role="status" key={entry.id}>Worker {entry.label}: {entry.error}</p>)}
  </section>;
}
