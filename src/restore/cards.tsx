import {useRef,useState} from 'react';
import type {AttentionCard} from '../desk/NeedsYou';
import type {WorkerState} from '../bridge';
import {PrivacyText} from '../desk/PrivacyText';
import {RealignButton} from '../commit/Realignment';
import type {RealignJob} from '../commit/realign';
import type {Retirement,RestoreControl,RestoreFollowup,RestoreJob} from './types';
import './restore.css';
function Followup({item,jobs,workers,disabled,changed,queued}:{item:RestoreFollowup;jobs:RealignJob[];workers:WorkerState[];disabled:boolean;changed:()=>Promise<void>;queued:(job:RealignJob)=>void}) {
  const [busy,setBusy]=useState(false),[error,setError]=useState('');
  const pending=useRef<RestoreControl|null>(null);
  async function dismiss(){if(disabled||busy)return;setBusy(true);setError('');try{pending.current??={opId:await window.intercat.createOperationId(),id:item.requestId,revision:item.revision,action:{kind:'dismissRealign',head:item.target}};await window.intercat.controlPathRestore(pending.current);pending.current=null;await changed();}catch(failure){setError(failure instanceof Error?failure.message:'The choice could not be confirmed.');}finally{setBusy(false);}}
  return <div className="restore-notice"><strong>Worker {item.label} is restored</strong><p>Realign its files with the current committed result?</p>{error&&<p role="alert"><PrivacyText text={error}/></p>}<div data-card-actions><button disabled={disabled||busy} onClick={()=>{void dismiss();}}>{pending.current?'Retry Not now':'Not now'}</button><RealignButton source={item.source} title={`Realign ${item.label}`} track={{id:item.track,label:item.label}} jobs={jobs} workers={workers} disabled={disabled||busy||!!pending.current} queued={queued}/></div></div>;
}
export function restoreCards({entries,jobs,followups,realignments,workers,disabled,open,changed,queued}:{entries:Retirement[];jobs:RestoreJob[];followups:RestoreFollowup[];realignments:RealignJob[];workers:WorkerState[];disabled:boolean;open:(entry:Retirement)=>void;changed:()=>Promise<void>;queued:(job:RealignJob)=>void}):AttentionCard[] {
  return [...jobs.filter(job=>job.phase!=='done').flatMap(job=>{
    const entry=entries.find(entry=>entry.id===job.request.retirementId);if(!entry)return [];
    const needs=['held','failed','waiting-for-access','waiting-for-processes','needs-merge'].includes(job.phase);
    const detail=job.phase==='needs-merge'?'Restore has conflicts':job.phase==='waiting-for-processes'?'Restore waits for running processes':job.phase==='waiting-for-access'?'Restore needs file access confirmation':job.phase==='held'?'Restore is held':job.phase==='failed'?'Restore needs attention':'Restore is in progress';
    return [{id:`restore-${job.request.opId}`,version:job.revision,kind:needs?'blocked' as const:'proposal' as const,title:`Restore Worker ${entry.label}`,content:<div className="restore-notice"><strong>Worker {entry.label} · {detail}</strong><p>Held requests remain held.</p><div data-card-actions><button disabled={disabled} onClick={()=>open(entry)}>Review Restore</button></div></div>}];
  }),...followups.map(item=>({id:`restore-realign-${item.requestId}`,version:`${item.revision}:${item.target}`,kind:'proposal' as const,title:`Realign restored Worker ${item.label}`,content:<Followup key={`${item.requestId}:${item.target}`} item={item} jobs={realignments} workers={workers} disabled={disabled} changed={changed} queued={queued}/>}))];
}
