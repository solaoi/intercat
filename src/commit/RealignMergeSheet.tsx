import {lazy,Suspense,useCallback,useRef,useState} from 'react';
import type {RepairRequest,RepairRun} from './repair';
import {realignMergeOwner,type RealignJob} from './realign';
import type {OutputFlow,SecretInputState} from '../bridge';
import {RepairPanel} from './RepairPanel';
import './commit.css';
const ManualMerge=lazy(()=>import('./ManualMerge'));

export default function RealignMergeSheet({job,run,label,flows,secrets,changed,close}:{job:RealignJob;run:RepairRun|undefined;label:string;flows:OutputFlow[];secrets:SecretInputState[];changed:()=>Promise<void>;close:()=>void}) {
  const shared=job.sharedInput,sourceLabel=shared?.label??label;
  const [busy,setBusy]=useState(false),[error,setError]=useState(''),[dirty,setDirty]=useState(false),[confirmHide,setConfirmHide]=useState(false);
  const pending=useRef<{request:RepairRequest;manual:boolean}|null>(null);
  const dirtyChanged=useCallback((value:boolean)=>setDirty(value),[]);
  const requestClose=()=>{if(dirty)setConfirmHide(true);else close();};
  async function action(work:()=>Promise<unknown>){setBusy(true);setError('');try{await work();await changed();}catch(failure){setError(failure instanceof Error?failure.message:'The merge action could not be confirmed.');}finally{setBusy(false);}}
  async function start(manual:boolean){await action(async()=>{
    pending.current??={manual,request:{opId:await window.intercat.createOperationId(),owner:realignMergeOwner(job),track:shared?.track??job.request.trackId,purpose:{kind:'merge'}}};
    if(pending.current.manual)await window.intercat.startManualMerge(pending.current.request);
    else {const response=await window.intercat.startRepair(pending.current.request);if(response.status==='rejected')throw new Error(response.reason);}
    pending.current=null;
  });}
  const active=run&&!['done','discarded'].includes(run.phase)?run:undefined;
  const held=!!active&&['held','failed'].includes(active.phase)&&!active.remaining&&!active.restoring;
  const canResume=active?.phase==='held'||active?.phase==='failed'&&active.failure==='capture';
  return <aside className="commit-sheet realign-merge-sheet" aria-label={shared?`Shared merge for ${sourceLabel}`:`Realignment merge for ${label}`} data-focus-region tabIndex={-1} onKeyDown={event=>{if(event.key==='Escape'&&!event.defaultPrevented&&!event.nativeEvent.isComposing){event.stopPropagation();requestClose();}}}>
    <div className="commit-scroll"><header className="commit-heading"><div><h2>{shared?`Shared changes from ${sourceLabel}`:`Realign ${label}`}</h2><p>{shared?`Checkpoint ${shared.checkpoint} · ${label} waits to realign`:'Resolve in an isolated workspace'}</p></div><button aria-label={shared?'Hide shared merge':'Hide realignment merge'} onClick={requestClose}>×</button></header>
      {confirmHide&&<section className="commit-refresh" role="group" aria-label="Unsaved realignment choices"><strong>The editor has unsaved resolution choices.</strong><p>Saved merge files stay available when this panel is hidden.</p><div><button onClick={()=>setConfirmHide(false)}>Keep editing</button><button onClick={close}>Discard editor drafts and hide</button></div></section>}
      {error&&<div className="commit-error" role="alert"><p>{error}</p>{pending.current&&<button disabled={busy} onClick={()=>{void start(pending.current!.manual);}}>Retry saved request</button>}</div>}
      <p className="commit-notice">{shared?`Resolve ${sourceLabel}’s shared checkpoint, then continue ${label}’s realignment.`:`The worker’s files remain unchanged until every conflict is resolved. Requests to ${label} wait during realignment.`}</p>
      {active?.phase==='manual'?<Suspense fallback={<p className="commit-empty">Opening conflict editor…</p>}><ManualMerge run={active} label={sourceLabel} targetLabel={shared?'chosen context':'target'} changed={changed} dirtyChanged={dirtyChanged}/></Suspense>:active?<RepairPanel key={active.id} run={active} label={sourceLabel} secretInputs={secrets} preview={flows.find(flow=>flow.repairRunId===active.id)?.preview} changed={changed}/>:<p className="commit-empty">Choose how to resolve this realignment.</p>}
    </div>
    {active?.phase!=='manual'&&<footer className="commit-footer"><p role="status">{busy?'Saving merge choice…':job.phase==='done'?'Realignment complete':job.phase==='needs-merge'?'Realignment has conflicts':'Realignment will continue at the worker’s idle boundary.'}</p><div>
      {!active&&job.phase==='needs-merge'&&<><button disabled={busy||!!pending.current} onClick={()=>{void action(async()=>window.intercat.controlRealign({opId:await window.intercat.createOperationId(),id:job.request.opId,revision:job.revision,action:{kind:'discard'}}));}}>Discard realignment</button><button disabled={busy||!!pending.current} onClick={()=>{void start(true);}}>Let me resolve</button><button disabled={busy||!!pending.current} onClick={()=>{void start(false);}}>{shared?'Let agent try':`Let ${label} try`}</button></>}
      {held&&active&&<><button disabled={busy} onClick={()=>{void action(async()=>window.intercat.controlRepair({opId:await window.intercat.createOperationId(),id:active.id,revision:active.revision,action:'discard'}));}}>Discard merge</button><button disabled={busy} onClick={()=>{void action(async()=>window.intercat.beginManualMerge({opId:await window.intercat.createOperationId(),id:active.id,revision:active.revision}));}}>Let me resolve</button><button disabled={busy||!canResume} onClick={()=>{void action(async()=>window.intercat.controlRepair({opId:await window.intercat.createOperationId(),id:active.id,revision:active.revision,action:'resume'}));}}>{shared?'Let agent try again':`Let ${label} try again`}</button></>}
      <button disabled={busy} onClick={requestClose}>Hide</button>
    </div></footer>}
  </aside>;
}
