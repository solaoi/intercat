import { useContext, useEffect, useRef, useState } from 'react';
import type { DaemonStatus } from '../bridge';
import { AttentionActive, type AttentionCard } from '../desk/NeedsYou';
import { PrivacyText } from '../desk/PrivacyText';
import { canPrepareFresh, freshCanCancel, freshCanRetry, freshPhaseText, type FreshCommand, type FreshJob, type FreshRequest } from './types';
import './fresh.css';

const message=(failure:unknown)=>failure instanceof Error?failure.message.replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/,''):String(failure);
export function FreshDialog({request,status,close,updated,attempted,openTrack,wasAttempted,commands}:{
  request:FreshRequest;status:DaemonStatus;close:()=>void;updated:(job:FreshJob)=>void;attempted:(request:FreshRequest)=>void;openTrack:(track:string)=>void;wasAttempted:boolean;commands:Map<string,FreshCommand>;
}) {
  const ref=useRef<HTMLDialogElement>(null);
  const progress=useRef<HTMLParagraphElement>(null);
  const mounted=useRef(true);
  const job=status.freshTracks?.find(job=>job.request.opId===request.opId);
  const worker=status.workers?.find(worker=>worker.trackId===request.trackId&&worker.sessionId===request.sessionId);
  const track=status.tracks?.find(track=>track.id===request.trackId);
  const next=status.tracks?.find(track=>track.id===job?.successorTrack);
  const [sent,setSent]=useState(!!job||wasAttempted);
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState('');
  const pending=useRef<FreshCommand|null>(commands.get(request.opId)??null);
  useEffect(()=>{
    mounted.current=true;
    const dialog=ref.current;
    const opener=document.activeElement instanceof HTMLElement?document.activeElement:null;
    dialog?.showModal();
    return()=>{mounted.current=false;dialog?.close();if(opener?.isConnected&&!(opener instanceof HTMLButtonElement&&opener.disabled))opener.focus({preventScroll:true});};
  },[]);
  function accept(value:FreshJob) {
    if(value.request.opId!==request.opId||value.request.trackId!==request.trackId||value.request.sessionId!==request.sessionId)throw new Error('The saved request belongs to another conversation.');
    updated(value);
  }
  async function prepare() {
    if(busy||!status.connected||status.dispatchPaused||!status.workerConfigured||!worker||!canPrepareFresh(worker))return;
    setBusy(true);setError('');setSent(true);attempted(request);
    try {const saved=await window.intercat.prepareFresh(request);if(mounted.current)accept(saved);}
    catch(failure){if(mounted.current)setError(message(failure));}
    finally{if(mounted.current)setBusy(false);}
  }
  async function command(action:FreshCommand['action']) {
    if(busy||!job||!status.connected)return;
    progress.current?.focus({preventScroll:true});
    setBusy(true);setError('');
    try {
      const change=pending.current??{opId:await window.intercat.createOperationId(),id:job.request.opId,revision:job.revision,action};
      pending.current=change;
      commands.set(request.opId,change);
      const saved=await window.intercat.commandFresh(change);
      if(!mounted.current)return;
      accept(saved);pending.current=null;commands.delete(request.opId);
    } catch(failure){if(mounted.current)setError(message(failure));}
    finally{if(mounted.current)setBusy(false);}
  }
  async function review() {
    setBusy(true);setError('');
    try {const saved=await window.intercat.readFresh({id:request.opId});if(mounted.current){accept(saved);pending.current=null;commands.delete(request.opId);}}
    catch(failure){if(mounted.current)setError(message(failure));}
    finally{if(mounted.current)setBusy(false);}
  }
  const blocked=busy||!status.connected;
  return <dialog ref={ref} className="fresh-dialog" aria-labelledby="fresh-title" onCancel={event=>{event.preventDefault();if(!busy)close();}}>
    <header><h2 id="fresh-title">Start a fresh conversation</h2><button aria-label="Close fresh conversation" disabled={busy} onClick={close}>×</button></header>
    <div className="fresh-body">
      <p className="fresh-source">Worker {track?.label??'?'} · <PrivacyText text={track?.title??'Original conversation'}/></p>
      <p>A new conversation starts with a summary and the current files. Future requests continue there. The original conversation stays readable.</p>
      {!job&&<p>The current step and checkpoint finish first. Any pending approval or question still needs your answer.</p>}
      <p ref={progress} className="fresh-progress" role="status" tabIndex={-1}>{job?freshPhaseText[job.phase]:''}</p>
      {job?.checkpoint&&<p className="fresh-point">Saved turn · {job.checkpoint.sha.slice(0,10)}</p>}
      {status.dispatchPaused&&<p>All turns are paused. Resume from the main controls to continue.</p>}
      {!status.workerConfigured&&!job&&<p>Connect workers before starting a fresh conversation.</p>}
      {job?.phase==='needs-access'&&job.inspection&&<section className="fresh-access" aria-label="Current file access">
        <p>Another app may be using these files. Wait, or confirm that the current files can be saved.</p>
        {job.inspection.processes.length>0&&<ul>{job.inspection.processes.map(item=><li key={item.fingerprint}><PrivacyText text={item.app}/> · {item.files?`${item.files} open ${item.files===1?'file':'files'}`:'using the working folder'}</li>)}</ul>}
        {job.inspection.unreadable>0&&<p>{job.inspection.unreadable} {job.inspection.unreadable===1?'process could':'processes could'} not be inspected.</p>}
      </section>}
      {job?.phase==='cancelling'&&<p>The request is saved. Earlier requests stay held while its processes finish.</p>}
      {job?.phase==='transferred'&&<p>{next?`Continue with Worker ${next.label}.`:'Continue in the new conversation.'} The original conversation stays available to read.</p>}
      {(error||job?.error)&&<div className="fresh-error" role="alert"><PrivacyText text={error||job?.error||''}/>{pending.current&&<p>Retry the same action, or review the latest saved state.</p>}</div>}
      {!status.connected&&<p>Reconnect to check the saved request.</p>}
    </div>
    <footer>
      {pending.current?<><button disabled={blocked} onClick={()=>{void review();}}>Review current state</button><button disabled={blocked} onClick={()=>{void command(pending.current!.action);}}>Retry last action</button></>:job?<>
        {freshCanCancel(job)&&<button disabled={blocked} onClick={()=>{void command({kind:'cancel'});}}>Cancel start fresh</button>}
        {job.phase==='needs-access'&&<><button disabled={busy} onClick={close}>Wait</button><button disabled={blocked||status.dispatchPaused||!job.inspection} onClick={()=>{if(job.inspection)void command({kind:'allow-access',fingerprint:job.inspection.fingerprint});}}>Go ahead</button></>}
        {freshCanRetry(job)&&<button disabled={blocked||status.dispatchPaused} onClick={()=>{void command({kind:'retry'});}}>{job.phase==='owner-held'?'Finish transfer':'Retry start fresh'}</button>}
        {job.phase==='transferred'&&next&&<button disabled={busy} onClick={()=>{openTrack(next.id);close();}}>Open {next.label}</button>}
        {['cancelled','cancelling'].includes(job.phase)&&<button disabled={busy} onClick={close}>Close</button>}
      </>:<button disabled={blocked||status.dispatchPaused||!status.workerConfigured||!worker||!canPrepareFresh(worker)} onClick={()=>{void prepare();}}>{busy?'Saving request…':sent?'Retry saving request':'Start fresh'}</button>}
    </footer>
  </dialog>;
}

function FreshAttention({job,open}:{job:FreshJob;open:()=>void}) {
  const active=useContext(AttentionActive);
  return <div className="desk-card desk-action-card"><div className="desk-copy"><strong>{freshPhaseText[job.phase]}</strong>{job.error&&<p><PrivacyText text={job.error}/></p>}</div><div className="desk-actions" data-card-actions><button disabled={!active} onClick={open}>Review start fresh</button></div></div>;
}
export function freshCards(status:DaemonStatus,open:(request:FreshRequest)=>void):AttentionCard[] {
  return (status.freshTracks??[]).filter(job=>['needs-access','files-held','summary-held','owner-held'].includes(job.phase)||job.phase==='cancelling'&&!!job.error).flatMap(job=>{
    const track=status.tracks?.find(track=>track.id===job.request.trackId);
    if(!track)return [];
    return [{id:`fresh-${job.request.opId}`,kind:'blocked' as const,version:job.revision,title:`Fresh conversation for ${track.label}`,destination:{trackId:track.id,label:track.label},content:<FreshAttention job={job} open={()=>open(job.request)}/>}];
  });
}
