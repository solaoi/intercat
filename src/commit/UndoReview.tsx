import {lazy, Suspense, useEffect, useRef, useState} from 'react';
import type {Candidate, Diff, IntegrationJob} from './types';

const DiffView=lazy(()=>import('./DiffView'));
type Undo=NonNullable<Candidate['undo']>;
export interface UndoFocus {job:string;path:string;sequence:number}

function UndoDetails({job,undo,focus}:{job:IntegrationJob;undo:Undo;focus:UndoFocus|null}) {
  const selected=focus&&undo.changes.some(change=>change.path===focus.path)?focus.path:undo.changes[0]?.path??'';
  const [file,setFile]=useState(selected);
  const [diff,setDiff]=useState<Diff|null>(null);
  const [error,setError]=useState('');
  useEffect(()=>{if(focus&&undo.changes.some(change=>change.path===focus.path))setFile(focus.path);},[focus,undo.sha]);
  useEffect(()=>{
    let cancelled=false;
    setDiff(null);setError('');
    if(file)void window.intercat.getIntegrationDiff({id:job.id,revision:job.revision,owner:{kind:'undo'},path:file})
      .then(value=>{if(!cancelled)setDiff(value);})
      .catch(failure=>{if(!cancelled)setError(failure instanceof Error?failure.message:'The undo diff is unavailable.');});
    return()=>{cancelled=true;};
  },[job.id,job.revision,undo.sha,file]);
  return <>
    <p className="commit-notice">Intercat creates this commit before the chosen path’s commits.</p>
    <pre className="commit-undo-message" aria-label="Undo commit message">{undo.message}</pre>
    <code className="commit-undo-sha" aria-label="Undo commit SHA">{undo.sha}</code>
    <div className="commit-files" role="group" aria-label="Undo changed files">{undo.changes.map(change=><button key={change.path} aria-pressed={file===change.path} onClick={()=>setFile(change.path)}><span>{change.before?change.after?'M':'D':'A'}</span>{change.path}</button>)}</div>
    {!undo.changes.length?<p className="commit-empty">No file changes in the undo commit.</p>:error?<p className="commit-error" role="alert">{error}</p>:diff?<>{diff.redacted&&<p className="commit-notice">Sensitive text is hidden in this preview.</p>}<Suspense fallback={<p className="commit-empty">Loading undo diff…</p>}><DiffView value={diff}/></Suspense></>:<p className="commit-empty">Loading undo diff…</p>}
  </>;
}

export function UndoReview({job,focus}:{job:IntegrationJob;focus:UndoFocus|null}) {
  const [open,setOpen]=useState(false);
  const host=useRef<HTMLDetailsElement>(null);
  useEffect(()=>{
    if(!focus)return;
    setOpen(true);
    const frame=requestAnimationFrame(()=>{host.current?.scrollIntoView({block:'nearest'});host.current?.focus({preventScroll:true});});
    return()=>cancelAnimationFrame(frame);
  },[focus]);
  const undo=job.candidate?.undo;
  if(!undo)return null;
  return <details ref={host} tabIndex={-1} open={open} className="commit-undo-review" onToggle={event=>setOpen(event.currentTarget.open)}>
    <summary>Undo commit · Intercat</summary>
    {open&&<UndoDetails key={`${job.id}:${job.revision}:${undo.sha}`} job={job} undo={undo} focus={focus}/>}
  </details>;
}
