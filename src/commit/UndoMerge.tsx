import {lazy,Suspense,useRef,useState} from 'react';
import type {OutputFlow,SecretInputState} from '../bridge';
import type {IntegrationJob} from './types';
import type {RepairRequest,RepairRun} from './repair';
import {RepairPanel} from './RepairPanel';
const ManualMerge=lazy(()=>import('./ManualMerge'));

export function UndoMerge({job,run,flows,secrets,changed,dirtyChanged}:{job:IntegrationJob;run:RepairRun|undefined;flows:OutputFlow[];secrets:SecretInputState[];changed:()=>Promise<void>;dirtyChanged:(value:boolean)=>void}) {
  const [busy,setBusy]=useState(false),[error,setError]=useState('');
  const pending=useRef<{request:RepairRequest;manual:boolean}|null>(null);
  const conflict=job.candidate?.undoConflict??job.plan?.undoConflict;
  if(!conflict&&!run)return null;
  const label=job.tracks.find(track=>track.id===(run?.track??conflict?.track))?.label??'?';
  async function action(work:()=>Promise<unknown>){setBusy(true);setError('');try{await work();await changed();}catch(failure){setError(failure instanceof Error?failure.message:'The undo merge could not be confirmed.');}finally{setBusy(false);}}
  async function start(manual:boolean){await action(async()=>{
    if(!conflict)return;
    pending.current??={manual,request:{opId:await window.intercat.createOperationId(),owner:{kind:'undo',id:job.id,revision:job.revision},track:conflict.track,purpose:{kind:'merge'}}};
    if(pending.current.manual)await window.intercat.startManualMerge(pending.current.request);
    else {const response=await window.intercat.startRepair(pending.current.request);if(response.status==='rejected')throw new Error(response.reason);}
    pending.current=null;
  });}
  const held=!!run&&['held','failed'].includes(run.phase)&&!run.remaining&&!run.restoring;
  return <section className="commit-paths" aria-label="Resolve undo conflict">
    <h3>Resolve the undo</h3><p>Preserve later edits while undoing {label}’s published changes.</p>
    {error&&<div className="commit-error" role="alert"><p>{error}</p>{pending.current&&<button disabled={busy} onClick={()=>{void start(pending.current!.manual);}}>Retry saved undo request</button>}</div>}
    {run?.phase==='manual'?<Suspense fallback={<p>Opening undo editor…</p>}><ManualMerge run={run} label="Undo" targetLabel="later edits" changed={changed} dirtyChanged={dirtyChanged}/></Suspense>:run?<RepairPanel run={run} label="Undo" secretInputs={secrets} preview={flows.find(flow=>flow.repairRunId===run.id)?.preview} changed={changed}/>:null}
    {!run&&<div className="commit-conflict-actions"><button disabled={busy||!!pending.current||!job.candidate?.undoConflict} onClick={()=>{void start(true);}}>Let me resolve undo</button><button disabled={busy||!!pending.current||!job.candidate?.undoConflict} onClick={()=>{void start(false);}}>Let {label} try undo</button></div>}
    {held&&run&&<div className="commit-conflict-actions">
      <button disabled={busy} onClick={()=>{void action(async()=>window.intercat.controlRepair({opId:await window.intercat.createOperationId(),id:run.id,revision:run.revision,action:'discard'}));}}>Discard undo merge</button>
      <button disabled={busy} onClick={()=>{void action(async()=>window.intercat.beginManualMerge({opId:await window.intercat.createOperationId(),id:run.id,revision:run.revision}));}}>Let me resolve undo</button>
      <button disabled={busy||run.phase==='failed'&&run.failure!=='capture'} onClick={()=>{void action(async()=>window.intercat.controlRepair({opId:await window.intercat.createOperationId(),id:run.id,revision:run.revision,action:'resume'}));}}>Let {label} try undo again</button>
    </div>}
  </section>;
}
