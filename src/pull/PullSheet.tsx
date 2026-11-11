import React,{useEffect,useRef,useState} from 'react';
import type {PullAction,PullCommand,PullDraft,PullJob,PullOpen,PullSelection} from './types';
import {pullDone,pullLabel,pullRunning} from './types';
import '../commit/commit.css';
import '../push/push.css';
import './pull.css';
type Pending={kind:'open';request:PullOpen}|{kind:'command';request:PullCommand};
export default function PullSheet({selection,jobs,connected,changed,close}:{selection:PullSelection;jobs:PullJob[];connected:boolean;changed:(job:PullJob)=>void;close:()=>void}){
  const [jobId,setJobId]=useState(selection.jobId);
  const [local,setLocal]=useState<PullJob|null>(null);
  const [destinations,setDestinations]=useState<Record<string,string>>(()=>Object.fromEntries(selection.review.members.map(member=>[member.folder.id,member.target.destinations[0]?.fingerprint??''])));
  const [bases,setBases]=useState<Record<string,string>>(()=>Object.fromEntries(selection.review.members.map(member=>[member.folder.id,member.folder.baseBranch?.replace(/^refs\/heads\//,'')??''])));
  const [drafts,setDrafts]=useState<Record<string,PullDraft>>({});
  const [dirty,setDirty]=useState(false);
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState('');
  const pending=useRef<Pending|null>(null);
  const heading=useRef<HTMLHeadingElement>(null);
  const observed=jobs.find(job=>job.id===jobId||!jobId&&job.request.pushReview===selection.review.request.opId);
  const job=observed&&(!local||observed.revision>=local.revision)?observed:local;
  const running=!!job&&pullRunning(job);
  const done=!!job&&pullDone(job);
  const editable=!!job&&!job.held&&!job.confirmed&&job.members.every(member=>member.phase==='review');
  useEffect(()=>{const previous=document.activeElement;heading.current?.focus();return()=>{if(previous instanceof HTMLElement&&previous.isConnected)previous.focus();};},[]);
  useEffect(()=>{if(job&&!dirty)setDrafts(Object.fromEntries(job.members.flatMap(member=>member.draft?[[member.folderId,member.draft]]:[])));},[job?.id,job?.revision,dirty]);
  async function execute(value:Pending){
    if(busy||!connected)return;
    pending.current=value;setBusy(true);setError('');
    try{const result=value.kind==='open'?await window.intercat.openPullRequest(value.request):await window.intercat.pullCommand(value.request);setJobId(result.id);setLocal(result);changed(result);setDirty(false);pending.current=null;}
    catch(failure){setError(failure instanceof Error?failure.message:'The PR choice could not be saved. Retry keeps the same operation.');}
    finally{setBusy(false);}
  }
  async function command(action:PullAction){await execute({kind:'command',request:{opId:await window.intercat.createOperationId(),action}});}
  async function prepare(){await execute({kind:'open',request:{opId:await window.intercat.createOperationId(),pushReview:selection.review.request.opId,choices:selection.review.members.map(member=>({folderId:member.folder.id,destination:destinations[member.folder.id],base:bases[member.folder.id]}))}});}
  async function openGitHub(folderId:string,kind:'pull'|'branch'){
    if(!job)return;
    try{await window.intercat.openPullOnGitHub({id:job.id,folderId,kind});}
    catch{setError('The saved GitHub link could not be opened. Its destination remains in this review.');}
  }
  const missingBase=selection.review.members.some(member=>!member.folder.baseBranch);
  const frozen=!!job||!!pending.current;
  const canReviewAgain=job&&!running&&job.members.every(member=>!member.result&&!['creating','uncertain','checking','discarded'].includes(member.phase));
  return <aside className="commit-sheet push-sheet pull-sheet" aria-label="Pull request review" data-focus-region tabIndex={-1} onKeyDown={event=>{if(event.key==='Escape'&&!event.nativeEvent.isComposing){event.stopPropagation();close();}}}>
    <header className="commit-heading"><div><h2 ref={heading} tabIndex={-1}>Open PR</h2><p>{job?.account?`GitHub · @${job.account.login}`:'Review before creating or updating'}</p></div><button aria-label="Hide PR review" onClick={close}>×</button></header>
    <div className="push-content">
      {!frozen&&<><p className="push-help">Prepare a description with the local Editor, then read your GitHub CLI account and matching PRs. Creating or updating needs your confirmation.</p>{selection.review.members.map(member=><section className="push-member pull-options" key={member.folder.id}><h3>{member.folder.name}</h3><label>Repository<select aria-label={`PR repository for ${member.folder.name}`} value={destinations[member.folder.id]} onChange={event=>setDestinations(values=>({...values,[member.folder.id]:event.target.value}))}>{member.target.destinations.map(destination=><option key={destination.fingerprint} value={destination.fingerprint}>{destination.display}</option>)}</select></label><label>Base branch<input aria-label={`PR base for ${member.folder.name}`} value={bases[member.folder.id]} disabled={!member.folder.baseBranch} onChange={event=>setBases(values=>({...values,[member.folder.id]:event.target.value}))}/></label>{!member.folder.baseBranch&&<p className="push-later">no base branch yet</p>}<dl><dt>Head</dt><dd>{member.target.targetRef.replace(/^refs\/heads\//,'')}</dd><dt>Commit</dt><dd><code>{member.target.sha}</code></dd></dl></section>)}</>}
      {job&&job.members.map(member=><section className="push-member" key={member.folderId} aria-label={`PR ${member.name}`}><h3>{member.name}<span>{pullLabel[member.phase]}</span></h3><dl><dt>Repository</dt><dd>{member.target.repository.owner}/{member.target.repository.name}</dd><dt>Branches</dt><dd>{member.target.head} → {member.target.base}</dd><dt>Commit</dt><dd><code>{member.target.sha}</code></dd></dl>{member.draft&&<div className="pull-draft"><label>Title<input aria-label={`PR title for ${member.name}`} value={drafts[member.folderId]?.title??member.draft.title} readOnly={!editable||busy||!!pending.current} maxLength={256} onChange={event=>{setDirty(true);setDrafts(values=>({...values,[member.folderId]:{...member.draft!,...values[member.folderId],title:event.target.value}}));}}/></label><label>Description<textarea aria-label={`PR description for ${member.name}`} value={drafts[member.folderId]?.body??member.draft.body} readOnly={!editable||busy||!!pending.current} onChange={event=>{setDirty(true);setDrafts(values=>({...values,[member.folderId]:{...member.draft!,...values[member.folderId],body:event.target.value}}));}}/></label></div>}{member.existing&&!member.result&&<p className="push-help">Updates existing PR #{member.existing.number}. Review its text before confirming.<button onClick={()=>{void openGitHub(member.folderId,'pull');}}>View existing PR</button></p>}{member.result&&<p className="pull-result">PR #{member.result.number}<code>{member.result.url}</code><button onClick={()=>{void openGitHub(member.folderId,'pull');}}>View on GitHub</button></p>}{member.error&&<p className="push-later" role="status">{member.error}</p>}{member.phase==='uncertain'&&<button onClick={()=>{void openGitHub(member.folderId,'branch');}}>Check branch on GitHub</button>}</section>)}
      {selection.review.members.length>1&&<p className="push-help">After every PR has a URL, Intercat adds a “Part 1 of {selection.review.members.length} · see …” line with links to the related PRs.</p>}
      {error&&<div className="commit-error" role="alert"><p>{error}</p>{pending.current&&<button disabled={busy||!connected} onClick={()=>{if(pending.current)void execute(pending.current);}}>Retry saved choice</button>}</div>}
    </div>
    <footer className="push-footer"><p role="status">{busy?'Saving choice…':done?'PRs ready':running?'Preparing PRs · you can hide this review':dirty?'Save and review the screened text before confirming.':job?.held?'Stopped · saved results are retained.':editable?'Confirm the account, repository, branches and text.':'GitHub.com · uses the selected gh login'}</p><div>
      {!frozen&&<button className="commit-primary" disabled={busy||!connected||missingBase||selection.review.members.some(member=>!destinations[member.folder.id]||!bases[member.folder.id].trim())} onClick={()=>{void prepare();}}>Prepare PR{selection.review.members.length>1?'s':''}</button>}
      {job&&editable&&dirty&&<button className="commit-primary" disabled={busy||!connected||!!pending.current} onClick={()=>{void command({action:'edit',id:job.id,revision:job.revision,drafts:job.members.map(member=>({folderId:member.folderId,draft:drafts[member.folderId]??member.draft!}))});}}>Save text for review</button>}
      {job&&editable&&!dirty&&<button className="commit-primary" disabled={busy||!connected||!!pending.current} onClick={()=>{void command({action:'confirm',id:job.id,revision:job.revision});}}>Create or update {job.members.length===1?'PR':`${job.members.length} PRs`}</button>}
      {job&&job.held&&!done&&<button disabled={busy||!connected||!!pending.current} onClick={()=>{void command({action:'retry',id:job.id,revision:job.revision});}}>{job.members.some(member=>member.phase==='uncertain')?'Check existing result':'Retry unfinished PRs'}</button>}
      {canReviewAgain&&<button disabled={busy||!connected||!!pending.current} onClick={()=>{void command({action:'reviewAgain',id:job!.id,revision:job!.revision});}}>Review account and PRs again</button>}
      {job&&running&&<button disabled={busy||!connected||!!pending.current} onClick={()=>{void command({action:'stop',id:job.id,revision:job.revision});}}>Stop</button>}
      <button onClick={close}>Hide</button>
    </div></footer>
  </aside>;
}
