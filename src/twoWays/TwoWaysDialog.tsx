import { useContext,useEffect,useRef,useState } from 'react';
import type { DaemonStatus } from '../bridge';
import { PrivacyText } from '../desk/PrivacyText';
import { AttentionActive,type AttentionCard } from '../desk/NeedsYou';
import { phaseText,preparing,type TwoWaysCommand,type TwoWaysJob } from './types';
import './twoWays.css';
const message=(error:unknown)=>error instanceof Error?error.message.replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/,''):String(error);
export function TwoWaysDialog({id,status,commands,close,updated,openTrack}:{id:string;status:DaemonStatus;commands:Map<string,TwoWaysCommand>;close:()=>void;updated:(job:TwoWaysJob)=>void;openTrack:(id:string)=>void}){
  const ref=useRef<HTMLDialogElement>(null),mounted=useRef(true);
  const pending=useRef(commands.get(id)??null);
  const [busy,setBusy]=useState(false),[error,setError]=useState('');
  const job=status.twoWays?.find(job=>job.request.opId===id);
  const currentJob=useRef(job);currentJob.current=job;
  const source=status.tracks?.find(track=>track.id===job?.sourceTrack);
  const branch=status.branches?.find(branch=>branch.request.opId===job?.branchId);
  const child=status.tracks?.find(track=>track.id===branch?.trackId);
  const word=status.utterances?.find(word=>word.id===job?.request.inputId);
  useEffect(()=>{mounted.current=true;const dialog=ref.current;const opener=document.activeElement instanceof HTMLElement?document.activeElement:null;dialog?.showModal();return()=>{mounted.current=false;dialog?.close();const fallback=currentJob.current?document.querySelector<HTMLElement>(`[data-utterance-id="${CSS.escape(currentJob.current.request.inputId)}"]`):null;const target=opener?.isConnected&&opener!==document.body&&!(opener instanceof HTMLButtonElement&&opener.disabled)?opener:fallback;target?.focus({preventScroll:true});};},[]);
  function accept(value:TwoWaysJob){if(value.request.opId!==id||job&&(value.request.inputId!==job.request.inputId||value.sourceTrack!==job.sourceTrack||value.sourceSession!==job.sourceSession))throw new Error('The paired response belongs to another instruction.');updated(value);}
  async function read(){setBusy(true);setError('');try{const value=await window.intercat.readTwoWays({id});if(mounted.current){accept(value);pending.current=null;commands.delete(id);}}catch(error){if(mounted.current)setError(message(error));}finally{if(mounted.current)setBusy(false);}}
  async function command(action:TwoWaysCommand['action']){
    if(busy||!job||!status.connected)return;setBusy(true);setError('');
    try{const change=pending.current??{opId:await window.intercat.createOperationId(),id,revision:job.revision,action};pending.current=change;commands.set(id,change);const value=await window.intercat.commandTwoWays(change);if(mounted.current){accept(value);pending.current=null;commands.delete(id);}}
    catch(error){if(mounted.current)setError(message(error));}finally{if(mounted.current)setBusy(false);}
  }
  async function finish(){setBusy(true);setError('');try{const value=await window.intercat.finishTwoWays({id});if(mounted.current)accept(value);}catch(error){if(mounted.current)setError(message(error));}finally{if(mounted.current)setBusy(false);}}
  useEffect(()=>{if(!job&&status.connected)void read();},[id,status.connected]);
  return <dialog ref={ref} className="two-ways-dialog" aria-labelledby="two-ways-title" onCancel={event=>{event.preventDefault();if(!busy)close();}}>
    <header><h2 id="two-ways-title">Try it two ways</h2><button aria-label="Close two-way preparation" disabled={busy} onClick={close}>×</button></header>
    <div className="two-ways-body">
      <p>{source?`Worker ${source.label}`:'The original path'} and a second path receive the same instruction and references. This uses about twice the usage. You choose the path to keep after comparing the results.</p>
      <p role="status">{job?phaseText[job.phase]:'Reading the saved request…'}</p>
      {job?.point?.kind==='initial'&&<p>Before the first step · both paths share the saved initial files.</p>}
      {job?.variation&&<p className="two-ways-variation">{child?.label??'The second path'} at effort {job.variation.alternate} · same model</p>}
      {job?.phase==='waiting'&&word?.decision?.boundary==='when-told'&&<p>Waiting for your Start on Worker {source?.label??'in the original path'}. Its timing stays as you chose.</p>}
      {job?.phase==='awaiting-receipt'&&<p>Confirm to save the second path’s record of protected values before it reads the inherited history.</p>}
      {job?.phase==='cancelling'&&<p>The cancellation is saved. Confirm to save any protection records from the second path before cancellation finishes. The original instruction keeps its timing and any Stop you chose.</p>}
      {job?.phase==='queued'&&<p>Both paths use normal worker controls. Stop either worker to hold its work.</p>}
      {(error||job?.error)&&<p role="alert"><PrivacyText text={error||job?.error||''}/></p>}
      {!status.connected&&<p>Reconnect to review the saved request.</p>}
      {status.dispatchPaused&&<p>Turns are stopped. Resume from the main controls to continue.</p>}
    </div>
    <footer>{pending.current?<><button disabled={busy||!status.connected} onClick={()=>{void read();}}>Review saved state</button><button disabled={busy||!status.connected||status.dispatchPaused&&pending.current.action!=='cancel'} onClick={()=>{void command(pending.current!.action);}}>Retry last action</button></>:job?<>
      {preparing(job)&&job.phase!=='cancelling'&&<button disabled={busy||!status.connected} onClick={()=>{void command('cancel');}}>Cancel two ways</button>}
      {job.phase==='awaiting-usage-confirmation'&&<button disabled={busy||!status.connected||status.dispatchPaused} onClick={()=>{void command('confirm-usage');}}>Run both · about twice the usage</button>}
      {job.phase==='held'&&<button disabled={busy||!status.connected||status.dispatchPaused} onClick={()=>{void command('retry');}}>Retry preparation</button>}
      {(job.phase==='awaiting-receipt'||job.phase==='cancelling')&&<button disabled={busy||!status.connected} onClick={()=>{void finish();}}>{job.phase==='cancelling'?'Confirm cancellation':'Confirm preparation'}</button>}
      {source&&<button disabled={busy} onClick={()=>{openTrack(source.id);close();}}>Open {source.label}</button>}
      {child&&job.phase==='queued'&&<button disabled={busy} onClick={()=>{openTrack(child.id);close();}}>Open {child.label}</button>}
    </>:<button disabled={busy||!status.connected} onClick={()=>{void read();}}>Retry reading request</button>}</footer>
  </dialog>;
}
function Attention({job,open}:{job:TwoWaysJob;open:()=>void}){const active=useContext(AttentionActive);return <div className="desk-card desk-action-card"><div className="desk-copy"><strong>{phaseText[job.phase]}</strong>{job.error&&<p><PrivacyText text={job.error}/></p>}</div><div className="desk-actions" data-card-actions><button disabled={!active} onClick={open}>Review two ways</button></div></div>;}
export function twoWaysCards(status:DaemonStatus,open:(id:string)=>void):AttentionCard[]{return (status.twoWays??[]).filter(job=>['held','awaiting-usage-confirmation','awaiting-receipt','cancelling'].includes(job.phase)).map(job=>({id:`two-ways-${job.request.opId}`,kind:'blocked',version:job.revision,title:'Two-way preparation',destination:{trackId:job.sourceTrack,label:status.tracks?.find(track=>track.id===job.sourceTrack)?.label??'?'},content:<Attention job={job} open={()=>open(job.request.opId)}/>}));}
