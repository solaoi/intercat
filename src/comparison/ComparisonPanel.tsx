import {lazy,Suspense,useEffect,useRef,useState} from 'react';
import type {Comparison,Control,Evidence,EvidenceSide,Open,Selection,Side} from './types';
import type {Diff} from '../commit/types';
import {PrivacyText} from '../desk/PrivacyText';
import './comparison.css';
const DiffView=lazy(()=>import('../commit/DiffView'));
type Pending={kind:'open';request:Open}|{kind:'control';request:Control};
function evidenceLines(value:EvidenceSide):string[]{
  const tests=[value.tests.added&&`${value.tests.added} added`,value.tests.deleted&&`${value.tests.deleted} deleted`,value.tests.modified&&`${value.tests.modified} modified`,value.skipAdded&&`${value.skipAdded} skip added`].filter(Boolean);
  return [...(tests.length?[`Changes tests · ${tests.join(', ')}`]:[]),...value.runner.map(path=>`Changes how tests run · ${path}`),...value.unreadable.map(path=>`Couldn’t inspect changes · ${path}`),...(value.commandUnparsed?["Couldn’t inspect the Check command’s file names."]:[])];
}
function resultLabel(side:Side):string{
  const result=side.outcome;
  if(!result)return 'Check not run yet';
  if(result.state==='passed'&&result.check)return result.check?.command===null?'No checks configured':'Check passed';
  if(result.state==='failed'&&result.check?.code!=null)return `Check failed · exit ${result.check?.code}`;
  return 'Check couldn’t finish';
}
export default function ComparisonPanel({selection,connected,close}:{selection:Selection;connected:boolean;close:()=>void}){
  const dialog=useRef<HTMLDialogElement>(null),heading=useRef<HTMLHeadingElement>(null),files=useRef<HTMLSelectElement>(null);
  const pending=useRef<Pending|null>(null),saving=useRef(false),opening=useRef(false);
  const [job,setJob]=useState<Comparison|null>(null),[evidence,setEvidence]=useState<Evidence|null>(null);
  const [busy,setBusy]=useState(false),[error,setError]=useState(''),[readError,setReadError]=useState(''),[evidenceError,setEvidenceError]=useState('');
  const [path,setPath]=useState(''),[showDiff,setShowDiff]=useState(false),[diff,setDiff]=useState<Diff|null>(null),[diffError,setDiffError]=useState('');
  const currentId=useRef<string|null>(null);currentId.current=job?.id??null;
  const observe=(next:Comparison,replace=false)=>setJob(current=>current&&(current.id!==next.id&&!replace||current.id===next.id&&current.revision>next.revision)?current:next);
  useEffect(()=>{
    const previous=document.activeElement;dialog.current?.showModal();heading.current?.focus();
    return()=>{dialog.current?.close();if(previous instanceof HTMLElement&&previous.isConnected)previous.focus();};
  },[]);
  async function execute(value:Pending){
    if(saving.current||!connected)return;
    saving.current=true;pending.current=value;setBusy(true);setError('');
    try{observe(value.kind==='open'?await window.intercat.openComparison(value.request):await window.intercat.controlComparison(value.request),value.kind==='open');pending.current=null;}
    catch(failure){setError(failure instanceof Error?failure.message:'The comparison choice could not be saved. Retry keeps the same request.');}
    finally{saving.current=false;setBusy(false);}
  }
  async function begin(){
    if(saving.current||!connected)return;
    try{
      const existing=job?.id??selection.jobId;
      if(existing){observe(await window.intercat.readComparison({id:existing}));setReadError('');}
      else await execute({kind:'open',request:{opId:await window.intercat.createOperationId(),parent:selection.parent,child:selection.child}});
    }catch(failure){setReadError(failure instanceof Error?failure.message:'The saved comparison could not be opened.');}
  }
  useEffect(()=>{if(connected&&!opening.current){opening.current=true;void begin();}},[connected]);
  useEffect(()=>{
    if(!job||!['ready','running'].includes(job.phase)||!connected)return;
    let stopped=false,timer:ReturnType<typeof setTimeout>;
    const read=async()=>{try{const next=await window.intercat.readComparison({id:job.id});if(!stopped){observe(next);setReadError('');}}catch{if(!stopped)setReadError('The saved comparison could not be read. Reconnect to continue.');}finally{if(!stopped)timer=setTimeout(()=>{void read();},600);}};
    timer=setTimeout(()=>{void read();},300);return()=>{stopped=true;clearTimeout(timer);};
  },[job?.id,job?.phase,connected]);
  async function readEvidence(id:string){
    try{const next=await window.intercat.comparisonEvidence({id});if(currentId.current!==id)return;setEvidence(next);setEvidenceError('');setPath(current=>next.changes.some(change=>change.path===current)?current:next.changes[0]?.path??'');}
    catch{if(currentId.current===id)setEvidenceError('Changes to tests and their settings could not be read.');}
  }
  useEffect(()=>{setEvidence(null);setEvidenceError('');setDiff(null);setShowDiff(false);if(job)void readEvidence(job.id);},[job?.id]);
  useEffect(()=>{
    if(!job||!showDiff||!path)return;
    let cancelled=false;setDiff(null);setDiffError('');
    window.intercat.comparisonDiff({id:job.id,basis:'paths',path}).then(value=>{if(!cancelled)setDiff(value);},()=>{if(!cancelled)setDiffError('This saved diff could not be read.');});
    return()=>{cancelled=true;};
  },[job?.id,path,showDiff]);
  async function control(action:Control['action']){
    if(!job)return;
    try{await execute({kind:'control',request:{opId:await window.intercat.createOperationId(),id:job.id,revision:job.revision,attempt:job.attempt,action}});}
    catch{setError('The comparison choice could not be prepared.');}
  }
  async function checkCurrentSettings(){
    if(!job||saving.current)return;
    try{await execute({kind:'open',request:{opId:await window.intercat.createOperationId(),parent:job.request.parent,child:job.request.child}});}
    catch{setError('The new comparison could not be prepared.');}
  }
  const running=job&&['ready','running'].includes(job.phase);
  return <dialog className="comparison-sheet" ref={dialog} data-comparison-id={job?.id} aria-label="Compare saved paths" data-focus-region onCancel={event=>{event.preventDefault();close();}} onKeyDown={event=>{if(event.key==='Escape')event.stopPropagation();}}>
    <header><div><h2 ref={heading} tabIndex={-1}>Compare saved paths</h2><p>{job?`${job.sides[0].source.label} / ${job.sides[1].source.label} · ${job.folder.name}`:'Opening the selected checkpoints…'}</p></div><button aria-label="Hide path comparison" onClick={close}>×</button></header>
    <div className="comparison-content">
      {job&&<><p className="comparison-summary" role="status">{job.summary.label.replace(/ · Compare the diffs$/,'')}{job.summary.label.endsWith(' · Compare the diffs')&&<> · <button disabled={!evidence?.changes.length} onClick={()=>{setShowDiff(true);files.current?.focus();}}>Compare the diffs</button></>}</p>{job.summary.reason&&<p className="comparison-note">{job.summary.reason}</p>}
      <div className="comparison-results">{job.sides.map((side,index)=><section key={side.source.point.trackId} aria-label={`Saved result for Worker ${side.source.label}`}>
        <h3>Worker {side.source.label}<span>{resultLabel(side)}</span></h3><p className="comparison-sha">Saved checkpoint <code>{side.source.point.sha.slice(0,12)}</code>{side.cached?' · saved Check reused':''}</p>
        {evidence&&evidenceLines(evidence.sides[index]).map(line=><p className="comparison-warning" key={line}><PrivacyText text={line}/></p>)}
        {side.outcome?.reason&&<p className="comparison-note"><PrivacyText text={side.outcome.reason}/></p>}
        {side.outcome?.check?.command&&<details><summary>Check output</summary><pre><PrivacyText text={`${side.outcome.check.stdout}${side.outcome.check.stderr}`}/></pre>{side.outcome.check.truncated&&<p className="comparison-note">Output shortened.</p>}</details>}
      </section>)}</div>
      {!evidence&&!evidenceError&&<p className="comparison-note">Reading changes to tests and their settings…</p>}
      {evidenceError&&<p className="comparison-error" role="alert">{evidenceError} <button disabled={!connected} onClick={()=>{if(job)void readEvidence(job.id);}}>Retry reading changes</button></p>}
      {evidence&&<section className="comparison-code" aria-label="Saved path differences"><label>File<select ref={files} aria-label="File to compare between paths" value={path} disabled={!evidence.changes.length} onChange={event=>{setPath(event.target.value);setShowDiff(true);}}>{evidence.changes.map(change=><option key={change.path} value={change.path}>{change.path}</option>)}</select></label>{!evidence.changes.length?<p className="comparison-note">The saved paths have identical files.</p>:!showDiff?<button onClick={()=>setShowDiff(true)}>Open selected diff</button>:<><p className="comparison-note">Worker {job.sides[0].source.label} → Worker {job.sides[1].source.label}</p>{diffError?<p className="comparison-error" role="alert">{diffError}</p>:diff?<Suspense fallback={<p className="comparison-note">Opening diff…</p>}><DiffView value={diff} split/></Suspense>:<p className="comparison-note">Reading the saved diff…</p>}</>}</section>}
      </>}
      {readError&&<p className="comparison-error" role="alert">{readError} <button disabled={!connected||busy} onClick={()=>{void begin();}}>Read saved comparison</button></p>}
      {error&&<p className="comparison-error" role="alert">{error} {pending.current&&<button disabled={!connected||busy} onClick={()=>{if(pending.current)void execute(pending.current);}}>Retry saved choice</button>}</p>}
    </div>
    <footer><p>{!connected?'Reconnect to read saved results.':busy?'Saving choice…':running?'Checks continue if you hide this comparison.':job?.phase==='held'?'Held · retry uses these saved checkpoints and settings.':'Choose the path in the Commit review after comparing.'}</p><div>{running&&<button disabled={!connected||busy||!!pending.current} onClick={()=>{void control('hold');}}>Stop checks</button>}{job?.phase==='held'&&<button disabled={!connected||busy||!!pending.current} onClick={()=>{void control('retry');}}>Retry unfinished checks</button>}{job&&!running&&<button disabled={!connected||busy||!!pending.current} onClick={()=>{void checkCurrentSettings();}}>Check with current settings</button>}<button onClick={close}>Back to path choices</button></div></footer>
  </dialog>;
}
