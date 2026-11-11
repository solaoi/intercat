import React,{useEffect,useRef,useState} from 'react';
import type {PushAction,PushCommand,PushJob,PushPresentation,PushRequest,PushSelection,PushReview} from './types';
import {phaseLabel,pushActive,pushDone} from './types';
import { binaryReviewRule } from '../commit/types';
import '../commit/commit.css';
import './push.css';
type Pending={kind:'review';request:PushRequest}|{kind:'command';request:PushCommand};
export default function PushSheet({folders,selection,jobs,connected,close,changed,openPull}:{folders:{id:string;name:string}[];selection:PushSelection;jobs:PushJob[];connected:boolean;close:()=>void;changed:(job:PushJob)=>void;openPull:(review:PushReview)=>void}) {
  const [selected,setSelected]=useState(selection.folders);
  const [remotes,setRemotes]=useState<Record<string,string[]>>({});
  const [remote,setRemote]=useState<Record<string,string>>({});
  const [presentation,setPresentation]=useState<PushPresentation|null>(null);
  const [jobId,setJobId]=useState(selection.jobId);
  const [localJob,setLocalJob]=useState<PushJob|null>(null);
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState('');
  const pending=useRef<Pending|null>(null);
  const heading=useRef<HTMLHeadingElement>(null);
  const observed=jobs.find(job=>job.id===jobId||!jobId&&presentation&&job.review.request.opId===presentation.review.request.opId);
  const job=observed&&(!localJob||observed.revision>=localJob.revision)?observed:localJob;
  const active=!!job&&pushActive(job);
  const done=!!job&&pushDone(job);
  const terminalRevision=job&&!active?job.revision:null;
  const presentedJob=useRef<string|null>(null);
  useEffect(()=>{const focus=document.activeElement;heading.current?.focus();return()=>{if(focus instanceof HTMLElement&&focus.isConnected)focus.focus();};},[]);
  useEffect(()=>{
    let current=true;
    void Promise.allSettled(folders.map(async folder=>{
      const names=await window.intercat.getPushRemotes({folderId:folder.id});
      if(current){setRemotes(values=>({...values,[folder.id]:names}));setRemote(values=>({...values,[folder.id]:values[folder.id]??(names.includes('origin')?'origin':names[0]??'')}));}
    })).then(results=>{if(current&&results.some(result=>result.status==='rejected'))setError('Some folder remotes could not be read. Check the saved project and retry.');});
    return()=>{current=false;};
  },[]);
  useEffect(()=>{
    if(!job)return;
    const key=`${job.id}:${terminalRevision??'active'}`;
    if(presentedJob.current===key)return;
    let current=true;presentedJob.current=key;
    void window.intercat.getPushPresentation({id:job.review.request.opId}).then(value=>{if(current)setPresentation(value);}).catch(()=>{if(current){presentedJob.current=null;setError('The saved Push review could not be read. Reopen it after reconnecting.');}});
    return()=>{current=false;};
  },[job?.id,terminalRevision]);
  async function execute(value:Pending) {
    if(busy||!connected)return;
    pending.current=value;setBusy(true);setError('');
    try {
      if(value.kind==='review'){setPresentation(await window.intercat.openPushReview(value.request));setJobId(undefined);setLocalJob(null);}
      else {const result=await window.intercat.pushCommand(value.request);setJobId(result.id);setLocalJob(result);changed(result);}
      pending.current=null;
    } catch(failure){setError(failure instanceof Error?failure.message:'The saved choice could not be confirmed. Retry uses the same operation.');}
    finally{setBusy(false);}
  }
  async function review(){if(!selected.length||selected.some(id=>!remote[id]))return;await execute({kind:'review',request:{opId:await window.intercat.createOperationId(),choices:selected.map(folderId=>({folderId,remote:remote[folderId],targetRef:null,through:selection.through?.[folderId]??null}))}});}
  async function command(action:PushAction){await execute({kind:'command',request:{opId:await window.intercat.createOperationId(),action}});}
  const frozen=!!presentation||!!job||!!pending.current;
  const members=presentation?.review.members??job?.review.members??[];
  const reviewPushed=!!presentation&&presentation.review.members.every(member=>member.target.destinations.every(destination=>jobs.some(job=>job.recipients.some(recipient=>recipient.folderId===member.folder.id&&recipient.destination.fingerprint===destination.fingerprint&&recipient.phase==='done'&&recipient.receipt?.sha===member.target.sha&&recipient.receipt.reference===member.target.targetRef))));
  const preparePull=selection.intent==='pull'&&reviewPushed;
  return <aside className="commit-sheet push-sheet" aria-label="Push review" data-focus-region tabIndex={-1} onKeyDown={event=>{if(event.key==='Escape'&&!event.nativeEvent.isComposing){event.stopPropagation();close();}}}>
    <header className="commit-heading"><div><h2 ref={heading} tabIndex={-1}>{selection.intent==='pull'?'Open PR · review Push':'Push'}</h2><p>{job?'Saved Push':presentation?'Review before sending':'Choose folders and remotes'}</p></div><button aria-label="Hide Push review" onClick={close}>×</button></header>
    <div className="push-content">
      {!frozen&&<section className="push-choices" aria-label="Push folders">{folders.map(folder=><div key={folder.id}><label><input type="checkbox" checked={selected.includes(folder.id)} disabled={busy} onChange={event=>setSelected(values=>event.target.checked?[...values,folder.id]:values.filter(id=>id!==folder.id))}/>{folder.name}</label><label>Remote<select aria-label={`Remote for ${folder.name}`} value={remote[folder.id]??''} disabled={busy||!remotes[folder.id]?.length} onChange={event=>setRemote(values=>({...values,[folder.id]:event.target.value}))}>{!remotes[folder.id]?.length&&<option value="">No remotes available</option>}{remotes[folder.id]?.map(name=><option key={name}>{name}</option>)}</select></label></div>)}</section>}
      {!frozen&&!folders.length&&<p className="push-help">Commit saved work before opening Push.</p>}
      {!frozen&&<p className="push-help">{selection.intent==='pull'?'Choose the repositories for PR. If this commit needs Push, you will confirm it first.':'Review fixes the commit and every destination before you send.'}</p>}
      {members.map(member=><section className="push-member" key={member.folder.id} aria-label={`Push ${member.folder.name}`}><h3>{member.folder.name}<span>{member.target.remote}</span></h3><dl><dt>Branch</dt><dd>{member.target.targetRef.replace(/^refs\/heads\//,'')}</dd><dt>Commit</dt><dd><code>{member.target.sha}</code></dd></dl>
        <h4>Git destinations</h4><ul>{member.target.destinations.map((destination,index)=><li key={`${destination.fingerprint}:${index}`}><code>{destination.display}</code>{destination.credentials&&<small>Authentication details hidden</small>}</li>)}</ul>
        <h4>LFS services</h4><ul>{member.lfs.map((binding,index)=><li key={index}><code>{binding.upload.display}</code>{binding.api.fingerprint!==binding.upload.fingerprint&&<code>{binding.api.display}</code>}</li>)}</ul><p className="push-help">Files may go to storage locations provided by this LFS service</p>
        {!!member.ssh.length&&<><h4>SSH connections</h4><ul>{member.ssh.map(binding=><li key={binding.destination.fingerprint}><code>{binding.host}:{binding.port}</code><small>User and host key checked at connection</small></li>)}</ul></>}
        <details className="push-commits" open><summary>Commits through {member.target.sha.slice(0,8)}</summary><ul>{(presentation?.commits[member.folder.id]??[member.target.sha]).map(sha=><li key={sha}><code>{sha}</code></li>)}</ul></details>
        {!!presentation?.additionalCommits[member.folder.id]&&<p className="push-later" role="status">{presentation.additionalCommits[member.folder.id]} commit{presentation.additionalCommits[member.folder.id]===1?'':'s'} added after you reviewed · not pushed</p>}
      </section>)}
      {job&&<section className="push-outcomes" aria-label="Push progress" aria-live="polite">{job.recipients.map((recipient,index)=><div key={index} data-phase={recipient.phase}><strong>{phaseLabel[recipient.phase]}</strong><code>{recipient.destination.display}</code>{recipient.reason&&<p>{recipient.reason}</p>}{recipient.scans.flatMap(scan=>scan.findings).map(finding=><p key={finding.id}>{finding.rule===binaryReviewRule?`Binary pattern scan incomplete · ${finding.path}`:`Secret scan · ${finding.path}:${finding.line} · ${finding.rule}`}</p>)}{recipient.scans.some(scan=>scan.unscannedLfs.length>0)&&<p>LFS binary files were checked for supplied secret values. Pattern detection does not cover their contents.</p>}</div>)}</section>}
      {error&&<div className="commit-error" role="alert"><p>{error}</p>{pending.current&&<div><button disabled={busy||!connected} onClick={()=>{if(pending.current)void execute(pending.current);}}>Retry saved choice</button>{pending.current.kind==='review'&&<button disabled={busy} onClick={()=>{pending.current=null;setError('');}}>Change selection</button>}</div>}</div>}
    </div>
    <footer className="push-footer"><p role="status">{busy?'Saving choice…':done?'Pushed':active?job?.stopping?'Stopping…':'Push in progress · you can hide this review':job?'Finished destinations stay saved.':presentation?'Ready for your confirmation':'Choose a remote for each selected folder.'}</p><div>
      {!frozen&&<button className="commit-primary" disabled={busy||!connected||!selected.length||selected.some(id=>!remote[id])} onClick={()=>{void review();}}>Review Push</button>}
      {presentation&&!job&&<><button disabled={busy||!!pending.current} onClick={()=>setPresentation(null)}>Change selection</button><button className="commit-primary" disabled={busy||!connected||!!pending.current} onClick={()=>{if(preparePull)openPull(presentation.review);else void command({action:'confirm',review:presentation.review.request.opId});}}>{preparePull?'Continue to PR review':<>Push to {members.reduce((sum,member)=>sum+member.target.destinations.length,0)} destination{members.reduce((sum,member)=>sum+member.target.destinations.length,0)===1?'':'s'}</>}</button></>}
      {job&&done&&<button className="commit-primary" disabled={busy||!connected||!!pending.current} onClick={()=>openPull(job.review)}>Open PR…</button>}
      {job&&!active&&!done&&!job.recipients.some(recipient=>recipient.phase==='needs-review')&&<button className="commit-primary" disabled={busy||!connected||!!pending.current} onClick={()=>{void command({action:'retry',id:job.id,revision:job.revision});}}>Retry unfinished destinations</button>}
      {job&&!active&&<button disabled={busy||!!pending.current} onClick={()=>{setPresentation(null);setLocalJob(null);setJobId(undefined);presentedJob.current=null;}}>New Push review</button>}
      {job&&active&&<button disabled={busy||!connected||job.stopping||!!pending.current} onClick={()=>{void command({action:'cancel',id:job.id,revision:job.revision});}}>Cancel Push</button>}
      <button onClick={close}>Hide</button>
    </div></footer>
  </aside>;
}
