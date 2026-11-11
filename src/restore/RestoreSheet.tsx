import {lazy,Suspense,useCallback,useEffect,useRef,useState} from 'react';
import type {OutputFlow,SecretInputState} from '../bridge';
import type {RepairRun} from '../commit/repair';
import {RepairPanel} from '../commit/RepairPanel';
import {PrivacyText} from '../desk/PrivacyText';
import type {AsideInfo,Retirement,RestoreControl,RestoreJob} from './types';
import '../commit/commit.css';
import './restore.css';
const ManualMerge=lazy(()=>import('../commit/ManualMerge'));

export default function RestoreSheet({entry,job,run,connected,available,operationId,flows,secrets,changed,close}:{entry:Retirement;job:RestoreJob|undefined;run:RepairRun|undefined;connected:boolean;available:boolean;operationId:string|undefined;flows:OutputFlow[];secrets:SecretInputState[];changed:()=>Promise<void>;close:()=>void}) {
  const [busy,setBusy]=useState(false),[error,setError]=useState(''),[dirty,setDirty]=useState(false),[confirmHide,setConfirmHide]=useState(false),[info,setInfo]=useState<AsideInfo|null>(null);
  const pending=useRef<{label:string;send:()=>Promise<unknown>}|null>(null);
  const dirtyChanged=useCallback((value:boolean)=>setDirty(value),[]);
  const requestClose=()=>{if(dirty)setConfirmHide(true);else close();};
  const active=run&&!['done','discarded'].includes(run.phase)?run:undefined;
  const processWait=job?.phase==='waiting-for-processes';
  useEffect(()=>{
    if(!processWait)return;
    let disposed=false,running=false;
    const read=async()=>{if(running)return;running=true;try{const value=await window.intercat.getAsideProcesses({retirementId:entry.id});if(!disposed)setInfo(value);}catch{if(!disposed)setInfo(null);}finally{running=false;}};
    void read();const timer=setInterval(()=>{void read();},1000);
    return()=>{disposed=true;clearInterval(timer);};
  },[entry.id,processWait]);
  async function save(label:string,build:()=>Promise<()=>Promise<unknown>>) {
    if(busy||!connected)return;setBusy(true);setError('');
    try{pending.current??={label,send:await build()};await pending.current.send();pending.current=null;await changed();}
    catch(failure){setError(failure instanceof Error?failure.message:'The Restore choice could not be confirmed.');}
    finally{setBusy(false);}
  }
  const control=(action:RestoreControl['action'])=>save('Restore choice',async()=>{
    if(!job)throw new Error('Read the Restore before choosing.');
    const request={opId:await window.intercat.createOperationId(),id:job.request.opId,revision:job.revision,action};return()=>window.intercat.controlPathRestore(request);
  });
  const start=(manual:boolean)=>save('merge request',async()=>{
    if(!job?.mergeInput)throw new Error('The saved Restore merge is not ready.');
    const request={opId:await window.intercat.createOperationId(),owner:{kind:'restore' as const,id:job.request.opId,revision:job.mergeInput.revision},track:job.trackId,purpose:{kind:'merge' as const}};
    return manual?()=>window.intercat.startManualMerge(request):async()=>{const response=await window.intercat.startRepair(request);if(response.status==='rejected')throw new Error(response.reason);return response;};
  });
  const blocked=busy||!connected||!!pending.current;
  const held=active&&['held','failed'].includes(active.phase)&&!active.remaining&&!active.restoring;
  const canResume=active?.phase==='held'||active?.phase==='failed'&&active.failure==='capture';
  const status=!job?!available?entry.phase!=='aside'?'This worker is still being set aside.':'This saved path is no longer set aside.':'Restore this worker to its original lane.':job.phase==='done'?`Worker ${entry.label} restored. Held requests remain held.`:job.phase==='held'?'Restore is held. The saved request remains available.':job.phase==='applying'?'Restoring files and saving a checkpoint…':job.phase==='needs-merge'?'Resolve every conflict before the worker’s files change.':job.phase==='waiting-for-access'?'Confirm file access before Restore continues.':processWait?'Restore waits for this worker’s processes.':job.phase==='failed'?'Restore needs attention. The saved request remains available.':'Restore will continue when this worker is idle.';
  return <aside className="commit-sheet restore-sheet" aria-label={`Restore Worker ${entry.label}`} data-restore-operation={operationId} data-focus-region tabIndex={-1} onKeyDown={event=>{if(event.key==='Escape'&&!event.defaultPrevented&&!event.nativeEvent.isComposing){event.stopPropagation();requestClose();}}}>
    <div className="commit-scroll"><header className="commit-heading"><div><h2>Restore Worker {entry.label}</h2><p>Same track · same saved session</p></div><button aria-label="Hide Restore" onClick={requestClose}>×</button></header>
      {confirmHide&&<section className="commit-refresh" role="group" aria-label="Unsaved Restore choices"><strong>The editor has unsaved resolution choices.</strong><p>Saved merge files stay available when this panel is hidden.</p><div><button onClick={()=>setConfirmHide(false)}>Keep editing</button><button onClick={close}>Discard editor drafts and hide</button></div></section>}
      {error&&<div className="commit-error" role="alert"><p><PrivacyText text={error}/></p>{pending.current&&<button disabled={busy||!connected} onClick={()=>{void save(pending.current!.label,async()=>pending.current!.send);}}>Retry saved {pending.current.label}</button>}</div>}
      <p className="commit-notice" role="status">{status}</p>
      {!job&&<p className="restore-summary">Restore the changes removed when this path was set aside. Held requests stay held until you choose to start them.</p>}
      {job?.error&&!['queued','waiting-for-worker','waiting-for-processes'].includes(job.phase)&&<p role="alert"><PrivacyText text={job.error}/></p>}
      {processWait&&<section className="restore-processes"><strong>Worker {entry.label} still has running processes</strong><p>{info?.groups!=null?`${info.groups} ${info.groups===1?'group':'groups'} still running`:'Checking this worker’s processes…'}</p>{info?.error&&<p role="alert"><PrivacyText text={info.error}/></p>}{info?.stop?.record.error&&<p role="alert"><PrivacyText text={info.stop.record.error}/></p>}<div className="restore-actions">
        <button disabled={blocked||job.keepProcesses} onClick={()=>{void control({kind:'keepProcesses'});}}>Keep (Restore waits)</button>
        <button disabled={blocked||!!info?.stop&&info.stop.state!=='complete'&&!info.stop.record.error} onClick={()=>{void save('process Stop',async()=>{const request=info?.stop?.record.request??{opId:await window.intercat.createOperationId(),retirementId:entry.id};return async()=>{const stop=await window.intercat.stopAsideProcesses(request);setInfo(current=>({retirementId:entry.id,groups:current?.groups??null,error:current?.error??null,stop}));return stop;};});}}>Stop it</button>
      </div></section>}
      {job?.phase==='waiting-for-access'&&job.inspection&&<section className="restore-processes"><strong>Some processes are using this worktree</strong><ul className="restore-access-list">{job.inspection.processes.map(item=><li key={item.fingerprint}><PrivacyText text={item.app}/> · {item.files?`${item.files} open ${item.files===1?'file':'files'}`:'working directory'}</li>)}</ul>{!!job.inspection.unreadable&&<p>{job.inspection.unreadable} {job.inspection.unreadable===1?'process':'processes'} could not be inspected.</p>}<div className="restore-actions"><button disabled={blocked} onClick={()=>{void control({kind:'hold'});}}>Wait</button><button disabled={blocked} onClick={()=>{void save('access confirmation',async()=>{const request={opId:await window.intercat.createOperationId(),request:job.request,fingerprint:job.inspection!.fingerprint};return()=>window.intercat.approvePathRestoreAccess(request);});}}>Go ahead</button></div></section>}
      {active?.phase==='manual'?<Suspense fallback={<p className="commit-empty">Opening conflict editor…</p>}><ManualMerge run={active} label={entry.label} targetLabel="restored context" changed={changed} dirtyChanged={dirtyChanged}/></Suspense>:active?<RepairPanel key={active.id} run={active} label={entry.label} secretInputs={secrets} preview={flows.find(flow=>flow.repairRunId===active.id)?.preview} changed={changed}/>:job?.phase==='needs-merge'&&<p className="commit-empty">Resolve in an isolated workspace. Worker {entry.label} keeps its current files until every stage is resolved.</p>}
    </div>
    {active?.phase!=='manual'&&<footer className="commit-footer"><p role="status">{busy?'Saving Restore choice…':job?.phase==='done'?'Restore complete':'Held requests remain held'}</p><div>
      {!job&&<button disabled={blocked||!available||entry.phase!=='aside'} onClick={()=>{void save('Restore request',async()=>{const request={opId:await window.intercat.createOperationId(),retirementId:entry.id};return()=>window.intercat.restorePath(request);});}}>Restore</button>}
      {job&&!active&&!['done','applying','held'].includes(job.phase)&&<button disabled={blocked} onClick={()=>{void control({kind:'hold'});}}>Hold Restore</button>}
      {job&&!active&&['held','failed'].includes(job.phase)&&<button disabled={blocked} onClick={()=>{void control({kind:'retry'});}}>{job.phase==='held'?'Continue Restore':'Retry Restore'}</button>}
      {job?.phase==='needs-merge'&&!active&&<><button disabled={blocked||!job.mergeInput} onClick={()=>{void start(true);}}>Let me resolve</button><button disabled={blocked||!job.mergeInput} onClick={()=>{void start(false);}}>Let {entry.label} try</button></>}
      {held&&active&&<><button disabled={blocked} onClick={()=>{void save('merge discard',async()=>{const request={opId:await window.intercat.createOperationId(),id:active.id,revision:active.revision,action:'discard' as const};return()=>window.intercat.controlRepair(request);});}}>Discard merge</button><button disabled={blocked} onClick={()=>{void save('manual merge',async()=>{const request={opId:await window.intercat.createOperationId(),id:active.id,revision:active.revision};return()=>window.intercat.beginManualMerge(request);});}}>Let me resolve</button><button disabled={blocked||!canResume} onClick={()=>{void save('merge resume',async()=>{const request={opId:await window.intercat.createOperationId(),id:active.id,revision:active.revision,action:'resume' as const};return()=>window.intercat.controlRepair(request);});}}>Let {entry.label} try again</button></>}
      <button disabled={busy} onClick={requestClose}>Hide</button>
    </div></footer>}
  </aside>;
}
