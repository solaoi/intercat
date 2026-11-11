import {useRef,useState} from 'react';
import type {RealignAction,RealignControl,RealignJob,RealignRequest,RealignSource,PendingRealignment} from './realign';
import type {LiveTrack,WorkerState} from '../bridge';
import type {AttentionCard} from '../desk/NeedsYou';
import {PrivacyText} from '../desk/PrivacyText';

function message(error:unknown):string {return (error instanceof Error?error.message:'The realignment could not be confirmed.').replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/,'');}
export function RealignButton({source,title,track,jobs,workers,disabled,queued}:{source:RealignSource;title:string;track:{id:string;label:string};jobs:RealignJob[];workers:WorkerState[];disabled:boolean;queued:(job:RealignJob)=>void}) {
  const [busy,setBusy]=useState(false),[error,setError]=useState('');
  const pending=useRef<RealignRequest|null>(null);
  const existing=jobs.find(item=>item.request.trackId===track.id&&!['done','discarded'].includes(item.phase));
  const done=jobs.some(item=>item.request.trackId===track.id&&item.request.source.kind===source.kind&&item.request.source.jobId===source.jobId&&item.request.source.revision===source.revision&&(source.kind!=='reparent'||item.request.source.kind==='reparent'&&item.request.source.parent===source.parent)&&item.phase==='done');
  const retryOwn=pending.current!==null&&existing?.request.opId===pending.current.opId;
  const connected=workers.some(worker=>worker.trackId===track.id&&worker.sessionId);
  async function request() {
    if(disabled&&!retryOwn||busy)return;setBusy(true);setError('');
    try {
      pending.current??={opId:await window.intercat.createOperationId(),source,trackId:track.id};
      const result=await window.intercat.requestRealign(pending.current);pending.current=null;queued(result);
    }catch(error){setError(message(error));}finally{setBusy(false);}
  }
  return <span className="realign-request"><button disabled={disabled&&!retryOwn||busy||!connected||done||!!existing&&!pending.current} title={!connected?'Connect this worker first':existing?'Use the realignment card to continue or discard':undefined} onClick={()=>{void request();}}>{pending.current?'Retry realignment request':done?`${track.label} realigned`:title}</button>{error&&<span role="alert"><PrivacyText text={error}/></span>}</span>;
}
function Realignment({job,label,worker,pendingFiles,disabled,changed,openMerge}:{job:RealignJob;label:string;worker:WorkerState|undefined;pendingFiles:boolean;disabled:boolean;changed:(job:RealignJob)=>void;openMerge:()=>void}) {
  const [busy,setBusy]=useState(false),[error,setError]=useState('');const pending=useRef<RealignControl|null>(null);
  async function decide(action:RealignAction) {
    if(disabled||busy)return;setBusy(true);setError('');
    try {
      pending.current??={opId:await window.intercat.createOperationId(),id:job.request.opId,revision:job.revision,action};
      changed(await window.intercat.controlRealign(pending.current));pending.current=null;
    }catch(error){setError(message(error));}finally{setBusy(false);}
  }
  const processes=['needs-processes','waiting-processes'].includes(job.phase);
  const access=['needs-access','waiting-access'].includes(job.phase);
  const merging=job.phase==='needs-merge';
  const working=job.phase==='preparing'||job.phase==='stopping-processes';
  const waitingTurn=job.phase==='queued'||job.phase==='waiting-for-worker';
  const blocked=disabled||busy||!!pending.current;
  const appNames=[...new Set(job.inspection?.processes.map(item=>item.app)??[])].slice(0,3).join(', ');
  const title=merging?job.sharedInput?`Shared changes from Worker ${job.sharedInput.label} need resolving`:`Worker ${label}’s realignment has conflicts`:processes?`Worker ${label} still has running processes`:access?appNames?`${appNames} ${job.inspection?.processes.some(item=>item.files)?'has files open in':'is using'} ${label}’s worktree`:'Some processes could not be inspected':working?`Realigning Worker ${label}`:waitingTurn?`Realignment queued for ${label}`:`Worker ${label}’s realignment needs attention`;
  return <div className="realignment"><strong><PrivacyText text={title}/></strong>
    <p>{merging?job.sharedInput?`Resolve ${job.sharedInput.label}’s shared checkpoint ${job.sharedInput.checkpoint}. ${label} waits to realign.`:'Resolve in an isolated workspace. This worker’s requests wait until realignment can continue.':processes?'Realignment waits. Requests can still go to this worker.':access?job.phase==='waiting-access'?'Waiting for file access to clear. Requests can still go to this worker.':'Confirm before changing this worker’s files.':working?job.stopCommand?'Stopping the remaining processes…':job.request.source.kind==='reparent'?'Applying the chosen parent and saving a checkpoint…':'Applying the committed result and saving a checkpoint…':waitingTurn?'Runs between this worker’s turns.':'The worker’s files remain available.'}</p>
    {access&&!!job.inspection?.unreadable&&<p>{job.inspection.unreadable} process{job.inspection.unreadable===1?'':'es'} could not be inspected.</p>}
    {(error||job.error)&&<p role="alert"><PrivacyText text={error||job.error!}/></p>}
    <div className="realignment-actions" data-card-actions>
      {pending.current&&error?<><button disabled={disabled||busy} onClick={()=>{pending.current=null;setError('');}}>Review current choices</button><button disabled={disabled||busy} onClick={()=>{void decide(pending.current!.action);}}>Retry saved choice</button></>:<>
      {!working&&!merging&&<button disabled={blocked||pendingFiles} onClick={()=>{void decide({kind:'discard'});}}>Discard realignment</button>}
      {merging?<button disabled={blocked} onClick={openMerge}>Review realignment merge</button>:processes?<><button disabled={blocked||job.phase==='waiting-processes'} onClick={()=>{void decide({kind:'keepProcesses'});}}>Keep (realign waits)</button><button disabled={blocked||!!worker?.activeTurn||!!worker?.checkpointPending} title={worker?.activeTurn?'Wait for the current turn to finish':undefined} onClick={()=>{void decide({kind:'stopProcesses'});}}>Stop it</button></>:access?<><button disabled={blocked||!job.inspection} onClick={()=>{if(job.inspection)void decide({kind:'goAhead',fingerprint:job.inspection.fingerprint});}}>Go ahead</button><button disabled={blocked||job.phase==='waiting-access'} onClick={()=>{void decide({kind:'wait'});}}>Wait</button></>:!working&&<>{!waitingTurn&&<button disabled={blocked} onClick={()=>{void decide({kind:'retry'});}}>{job.phase==='held'?'Continue realignment':'Retry realignment'}</button>}</>}
      </>}
    </div>
  </div>;
}
export function realignmentCards(jobs:RealignJob[],pending:PendingRealignment[],tracks:LiveTrack[],workers:WorkerState[],disabled:boolean,changed:(job:RealignJob)=>void,openMerge:(id:string)=>void):AttentionCard[] {
  return jobs.filter(job=>!['done','discarded'].includes(job.phase)).map(job=>{
    const label=tracks.find(track=>track.id===job.request.trackId)?.label??'?';
    return {id:`realign-${job.request.opId}`,version:String(job.revision),kind:['failed','held','needs-processes','needs-access','needs-merge'].includes(job.phase)?'blocked':'proposal',title:`Realignment for Worker ${label}`,content:<Realignment key={job.request.opId} job={job} label={label} worker={workers.find(worker=>worker.trackId===job.request.trackId)} pendingFiles={pending.some(item=>item.request.opId===job.request.opId)} disabled={disabled} changed={changed} openMerge={()=>openMerge(job.request.opId)}/>};
  });
}
