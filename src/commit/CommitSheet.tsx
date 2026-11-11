import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import type { Effect, OutputFlow, HostMemoryStatus, ChecksChange } from '../bridge';
import { Effects } from '../desk/Effects';
import { binaryReviewRule, type Diff, type IntegrationAction, type IntegrationJob, type IntegrationRequest } from './types';
import { Assumptions } from './Assumptions';
import { CommitOrder } from './CommitOrder';
import { RepairPanel } from './RepairPanel';
import { RepairAttribution, RepairOwnerChoices } from './RepairOwnership';
import type { RepairPurpose, RepairRequest, RepairRun, RepairRefreshRequest } from './repair';
import { bindContext, type CommitContext, type DiffSelection } from './context';
import { isInstructionFile } from '../instructionFiles';
import { PrivacyText } from '../desk/PrivacyText';
import { RealignButton } from './Realignment';
import type { RealignJob } from './realign';
import {useCommitGroup} from './useCommitGroup';
import {groupReadiness,type CommitGroup} from './group';
import {PathChoices} from './PathChoices';
import {UndoMerge} from './UndoMerge';
import {UndoReview,type UndoFocus} from './UndoReview';
import './commit.css';

const DiffView = lazy(() => import('./DiffView'));
const ManualMerge = lazy(() => import('./ManualMerge'));
type Draft = { text: string; revision: number };
export default function CommitSheet({ comparisons,connected,realignments, workers, realignChanged, hostMemory, initialRequest, initialJobId, jobs, groups, effects, repairs, flows, secretInputs, update, close, selectContext, folders = [] }: { comparisons:import('../comparison/types').Comparison[];connected:boolean;realignments:RealignJob[];workers:import('../bridge').WorkerState[];realignChanged:(job:RealignJob)=>void;hostMemory?: HostMemoryStatus; initialRequest?: IntegrationRequest; initialJobId?:string; jobs:IntegrationJob[]; groups:CommitGroup[]; folders?: { id: string; name: string; tracks: string[] }[]; secretInputs: import('../bridge').SecretInputState[]; effects: Effect[]; repairs: RepairRun[]; flows: OutputFlow[]; update: () => Promise<void>; close: () => void; selectContext: (context: CommitContext) => void }) {
  const [selectedJobId,setSelectedJobId] = useState(initialJobId);
  const job = jobs.find(job=>job.id===selectedJobId)??null;
  const [allFolders] = useState(folders.length>1&&!initialJobId&&(!initialRequest||initialRequest.action.action==='open'&&(initialRequest.action.tracks===null||initialRequest.action.tracks.length>1)));
  const [localBusy, setBusy] = useState(true);
  const group = useCommitGroup({enabled:allFolders,initialOperation:initialRequest?.opId,tracks:initialRequest?.action.action==='open'?initialRequest.action.tracks:null,groups,jobs,update,select:setSelectedJobId,close});
  const groupLocked=!!group.pending||group.group?.status==='committing';
  const busy=localBusy||allFolders&&(group.busy||groupLocked);
  const [error, setError] = useState('');
  const [trackId, setTrackId] = useState('');
  const [file, setFile] = useState('');
  const [tab, setTab] = useState<'changes' | 'message' | 'check'>(job?.status==='failed'&&['check-failed','check-mutated'].includes(job.candidate?.state??'')?'check':'changes');
  const [diff, setDiff] = useState<Diff | null>(null);
  const [diffError, setDiffError] = useState('');
  const [undoFocus,setUndoFocus]=useState<UndoFocus|null>(null);
  const [mergeDirty,setMergeDirty]=useState(false);
  const [confirmHide,setConfirmHide]=useState(false);
  const diffBinding = useRef<{ job: IntegrationJob; track: string; change: import('./types').Change } | null>(null);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [refreshChoice, setRefreshChoice] = useState<{ id: string; revision: number; repairId: string | null; label: string } | null>(null);
  const pending = useRef<IntegrationRequest | null>(null);
  const opening = useRef(false);
  const pendingRepair = useRef<RepairRequest | null>(null);
  const pendingRefresh = useRef<RepairRefreshRequest | null>(null);
  const pendingHistory = useRef<ChecksChange | null>(null);
  const pendingManual = useRef<import('./manual').Begin | RepairRequest | null>(null);
  const repair = repairs.find(run => run.owner.kind==='commit'&&run.owner.id === job?.id && (!['done', 'discarded'].includes(run.phase) || (run.refresh && !run.refresh.error)));
  const undoRepair=repairs.find(run=>run.owner.kind==='undo'&&run.owner.id===job?.id&&!['done','discarded'].includes(run.phase));
  const latestRepair = repair ?? (Object.keys(job?.contributions ?? {}).length ? repairs.find(run => run.owner.kind==='commit'&&run.owner.id === job?.id && run.phase === 'done') : undefined);
  const pathRealigning=!!job&&realignments.some(item=>item.request.source.kind==='reparent'&&item.request.source.jobId===job.id&&!['done','discarded'].includes(item.phase));
  const locked = pathRealigning || !!repair || !!undoRepair || !!job && ['committing', 'commit-blocked', 'done'].includes(job.status);
  const preparingReview = useRef<string|null>(null);
  const undoChecking=repairs.some(run=>run.owner.kind==='undo'&&run.owner.id===job?.id&&run.phase==='done'&&run.owner.revision+1===job.revision);
  useEffect(()=>{
    if(!job||job.status!=='preparing'||busy||locked||undoChecking||pending.current)return;
    const key=`${job.id}:${job.revision}`;
    if(preparingReview.current===key)return;
    preparingReview.current=key;
    void execute({action:'open',tracks:job.tracks.filter(track=>track.included&&track.checkpoint).map(track=>track.id)});
  },[job?.id,job?.revision,job?.status,busy,locked,undoChecking]);
  const refreshBusy = !!repair?.refresh && !repair.refresh.error;
  function requestClose(){if(mergeDirty)setConfirmHide(true);else close();}

  function requestRefresh() {
    if (!job) return;
    if (repair || Object.keys(job.contributions).length) {
      setRefreshChoice({ id: job.id, revision: job.revision, repairId: repair?.id ?? null, label: job.tracks.find(track => track.id === repair?.track)?.label ?? '' });
    } else void execute({ action: 'refresh', id: job.id, revision: job.revision });
  }
  async function confirmRefresh(retry?: RepairRefreshRequest) {
    const choice = refreshChoice;
    if (!choice && !retry) return;
    if (choice && !choice.repairId && !retry) {
      setRefreshChoice(null);
      await execute({ action: 'refresh', id: choice.id, revision: choice.revision });
      return;
    }
    setBusy(true); setError('');
    try {
      const request = retry ?? { opId: await window.intercat.createOperationId(), jobId: choice!.id, revision: choice!.revision, repairId: choice!.repairId! };
      pendingRefresh.current = request;
      const response = await window.intercat.refreshRepair(request);
      if (response.status === 'rejected') { pendingRefresh.current = null; setError(response.reason); return; }
      pendingRefresh.current = null; setRefreshChoice(null);
      await reload();
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The Refresh could not be confirmed.'); }
    finally { setBusy(false); }
  }

  async function execute(action: IntegrationAction, retry?: IntegrationRequest): Promise<IntegrationJob | null> {
    setBusy(true); setError('');
    try {
      let request = retry ?? { opId: await window.intercat.createOperationId(), action };
      let result: IntegrationJob;
      for (;;) {
        pending.current = request;
        result = await window.intercat.integrationCommand(request);
        await update();
        setSelectedJobId(result.id);
        if (['open', 'select', 'refresh', 'order', 'choosePath'].includes(request.action.action) && (result.status === 'open' && result.plan?.tree || result.status === 'merge-conflict' && (result.plan?.undoConflict || result.plan?.rows.some(row => row.state === 'conflict')))) {
          request = { opId: await window.intercat.createOperationId(), action: { action: 'prepare', id: result.id, revision: result.revision } };
        } else break;
      }
      pending.current = null;
      return result;
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The action could not be confirmed.'); return null; }
    finally { setBusy(false); }
  }
  async function enableHistory() {
    if (!job || busy || locked) return;
    setBusy(true); setError('');
    try {
      if (!pendingHistory.current) {
        const current = await window.intercat.getChecks();
        const choice = current.setting.folders[job.project.id];
        pendingHistory.current = { opId: await window.intercat.createOperationId(), revision: current.setting.revision, folder: job.project.id, command: choice?.command ?? { mode: 'auto' }, history: true };
      }
      await window.intercat.changeChecks(pendingHistory.current);
      pendingHistory.current = null;
      await execute({ action: 'recheck', id: job.id, revision: job.revision });
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'Git history could not be enabled.'); }
    finally { setBusy(false); }
  }
  async function openFolder(id:string) {
    const folder = folders.find(folder=>folder.id===id);
    if (!folder) return;
    const saved = jobs.find(job=>job.project.id===id && job.status!=='discarded');
    if (saved && (saved.status!=='done' || !folder.tracks.length)) {
      setSelectedJobId(saved.id);setError('');
      if(saved.status==='open'&&saved.plan?.tree||saved.status==='merge-conflict'&&saved.plan?.rows.some(row=>row.state==='conflict')) await execute({action:'prepare',id:saved.id,revision:saved.revision});
      else setBusy(false);
      return;
    }
    const action:IntegrationAction={action:'open',tracks:folder.tracks};
    const retry=pending.current?.action.action==='open'?pending.current:undefined;
    await execute(action,retry);
  }
  useEffect(() => {
    if (opening.current) return;
    opening.current=true;
    if(allFolders)setBusy(false);
    else if(initialRequest)void execute(initialRequest.action,initialRequest);
    else if(initialJobId) {setBusy(false);if(!jobs.some(job=>job.id===initialJobId))setError('This saved review is unavailable.');}
    else if(folders.length===1)void openFolder(folders[0].id);
    else {setBusy(false);setError('Choose a project folder before opening Commit.');}
  }, []);
  useEffect(() => {
    if (job && !job.tracks.some(track => track.id === trackId)) setTrackId(job.candidate?.failedTrack ?? job.tracks.find(track => track.included)?.id ?? job.tracks[0]?.id ?? '');
  }, [job, trackId]);
  const track = job?.tracks.find(track => track.id === trackId);
  const row = job?.plan?.rows.find(row => row.track === trackId);
  const commit = job?.candidate?.blockedCommit?.track === trackId ? job.candidate.blockedCommit : job?.candidate?.commits.find(commit => commit.track === trackId);
  const changes = commit?.changes ?? row?.changes ?? [];
  const fileNames = changes.map(change => change.path).join('\0');
  useEffect(() => { if (!changes.some(change => change.path === file)) setFile(changes[0]?.path ?? ''); }, [fileNames, file]);
  useEffect(() => {
    let cancelled = false;
    setDiff(null); setDiffError('');
    diffBinding.current = null;
    const change = changes.find(change => change.path === file);
    if (job && track && file) void window.intercat.getIntegrationDiff({ id: job.id, revision: job.revision, owner: {kind:'worker',track:track.id}, path: file }).then(value => { if (!cancelled) { setDiff(value); if (change) diffBinding.current = { job, track: track.id, change }; } }).catch(failure => { if (!cancelled) setDiffError(failure instanceof Error ? failure.message : 'The diff is unavailable.'); });
    return () => { cancelled = true; };
  }, [job?.id, job?.revision, commit?.sha, trackId, file]);

  async function reload() {
    try { await update(); pending.current = null; setError(''); }
    catch { setError('The current review could not be loaded.'); }
  }
  async function stopActiveRun(id:string) {
    try {await window.intercat.stopRepair({opId:await window.intercat.createOperationId(),id});await reload();}
    catch(failure){setError(failure instanceof Error?failure.message:'The integration Run could not be stopped.');}
  }
  async function askRepair(owner: string, purpose: RepairPurpose, retry?: RepairRequest) {
    if (!job) return;
    setBusy(true); setError('');
    try {
      const request = retry ?? { opId: await window.intercat.createOperationId(), owner:{kind:'commit' as const,id:job.id,revision:job.revision}, track: owner, purpose };
      pendingRepair.current = request;
      const response = await window.intercat.startRepair(request);
      if (response.status === 'rejected') { pendingRepair.current = null; setError(response.reason); return; }
      pendingRepair.current = null;
      await reload();
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The repair request could not be confirmed.'); }
    finally { setBusy(false); }
  }
  async function resolveManually() {
    if(!job||!track)return;setBusy(true);setError('');
    try {
      const request=pendingManual.current??(repair?{opId:await window.intercat.createOperationId(),id:repair.id,revision:repair.revision}:{opId:await window.intercat.createOperationId(),owner:{kind:'commit' as const,id:job.id,revision:job.revision},track:track.id,purpose:{kind:'merge' as const}});
      pendingManual.current=request;
      if('owner' in request) await window.intercat.startManualMerge(request); else await window.intercat.beginManualMerge(request);
      pendingManual.current=null;await reload();
    } catch(failure){setError(failure instanceof Error?failure.message:'The conflict editor could not be opened.');}
    finally {setBusy(false);}
  }
  async function mergeChoice(kind:'resume'|'discard') {
    if(!job||!track)return;setBusy(true);setError('');
    try {
      if(repair)await window.intercat.controlRepair({opId:await window.intercat.createOperationId(),id:repair.id,revision:repair.revision,action:kind});
      else if(kind==='resume'){await askRepair(track.id,{kind:'merge'});return;}
      if(kind==='discard')await execute({action:'select',id:job.id,revision:job.revision,track:track.id,included:false});
      await reload();
    } catch(failure){setError(failure instanceof Error?failure.message:'The merge choice could not be confirmed.');}
    finally {setBusy(false);}
  }
  const candidate = job?.candidate;
  function selectedLines(range: DiffSelection) {
    const binding = diffBinding.current;
    if (!binding || !['checked', 'failed'].includes(binding.job.status)) return;
    const context = bindContext(binding.job, binding.track, binding.change, range);
    if (context) selectContext(context);
  }
  const scans = candidate ? [...(candidate.undo ? [candidate.undo.scan] : []), ...candidate.secretScans] : [];
  const findings = scans.flatMap(scan => scan.findings).filter(finding => !job?.keptSecrets.includes(finding.id));
  const binaryReviews = findings.filter(finding => finding.rule === binaryReviewRule);
  const secretFindings = findings.filter(finding => finding.rule !== binaryReviewRule);
  const keptBinaries = scans.flatMap(scan => scan.findings)
    .filter(finding => finding.rule === binaryReviewRule && job?.keptSecrets.includes(finding.id)).length;
  const keptSecrets = (job?.keptSecrets.length ?? 0) - keptBinaries;
  const undoFinding=(id:string)=>candidate?.undo?.scan.findings.some(finding=>finding.id===id)??false;
  const unscannedLfs = [...new Set(scans.flatMap(scan => scan.unscannedLfs))];
  const instructionFiles = [...new Set((candidate
    ? [...candidate.commits, ...(candidate.blockedCommit ? [candidate.blockedCommit] : [])]
      .filter(commit => job?.tracks.some(track => track.id === commit.track && track.included))
      .flatMap(commit => commit.changes.map(change => change.path))
    : job?.plan?.rows.filter(row => job.tracks.some(track => track.id === row.track && track.included)).flatMap(row => row.changes.map(change => change.path)) ?? [])
    .filter(isInstructionFile))].sort();
  const dirty = job?.tracks.some(track=>!!drafts[track.id])??false;
  const draft = track ? drafts[track.id] : undefined;
  const pendingAssumptions = job?.assumptions.filter(assumption => assumption.included && assumption.state === 'open').length ?? 0;
  const canCommit = !job?.assumptionsPending && !job?.assumptionsFailed && !job?.assumptionsChanged && !busy && !locked && !pending.current && !dirty && job?.status === 'checked' && candidate && (!job.pathReview.families.length || job.pathState?.ready) && job.tracks.some(track => track.included && track.checkpoint);
  const effectsForJob = effects.filter(effect => job?.tracks.some(track => track.id === effect.trackId));

  const readiness=group.group?groupReadiness(group.group,jobs,repairs,realignments,new Set(Object.keys(drafts))):null;
  const groupAssumptions=jobs.filter(job=>group.group?.members.some(member=>member.jobId===job.id)&&job.status!=='done').reduce((count,job)=>count+job.assumptions.filter(assumption=>assumption.included&&assumption.state==='open').length,0);
  const folderNavigation = folders.length>1&&<label className="commit-folder-switch">Folder <select aria-label="Commit review folder" value={allFolders?group.folder??'':job?.project.id??''} disabled={localBusy||group.busy||mergeDirty||!!refreshChoice||confirmHide||!!pending.current||!!pendingManual.current||!!pendingRepair.current||!!pendingRefresh.current||!!pendingHistory.current} onChange={event=>{if(allFolders)group.choose(event.target.value);else void openFolder(event.target.value);}}>{folders.map(folder=><option key={folder.id} value={folder.id} disabled={!allFolders&&!folder.tracks.length&&!jobs.some(job=>job.project.id===folder.id)}>{folder.name}</option>)}</select></label>;
  const groupSummary=allFolders&&<section className="commit-group-summary" aria-label="All-folder review"><strong>Commit · {folders.length} folders</strong><p>Folder selection changes the details shown. Commit includes every checked folder below.</p>{readiness&&<ul>{readiness.members.map(member=><li key={member.folderId}><span>{member.name}</span><small>{member.state}</small></li>)}</ul>}{group.error&&<div className="commit-error" role="alert"><p>{group.error}</p><div><button disabled={group.busy} onClick={()=>{void group.retry();}}>Retry saved choice</button>{group.pending?.kind==='commit'&&<button disabled={group.busy} onClick={()=>{void group.reviewCurrent();}}>Review current group</button>}</div></div>}</section>;

  const individualFooter = job&&repair?.phase!=='manual'&&<footer className="commit-footer"><p role="status">{busy ? 'Saving review…' : job.status === 'done' ? `Committed · ${candidate?.head.slice(0, 8)}` : job.status === 'choosing' ? 'Choose a path and review its affected dependencies.' : job.status === 'committing' ? 'Committing… · wait' : job.status === 'building' || job.status === 'staging' ? 'Building candidate and checking…' : dirty ? 'Save or discard message drafts before committing.' : job.status === 'checked' ? candidate?.check?.command ? 'Checked and ready' : 'Ready · no check configured' : 'Review needs attention'}</p><div>
        {job.status === 'done' ? <>{track?.included&&<RealignButton key={`${job.id}:${track.id}`} source={{kind:'commit',jobId:job.id,revision:job.revision}} title={`Realign ${track.label}`} track={track} jobs={realignments} workers={workers} disabled={busy} queued={queued=>{realignChanged(queued);close();}}/>}{job.project.baseBranch === null && <><span>no base branch yet</span><button disabled title="no base branch yet">Open PR</button></>}<button onClick={close}>Done</button></> : <><button disabled={busy || locked} onClick={() => { void execute({ action: 'discard', id: job.id, revision: job.revision }).then(result => { if (result) close(); }); }}>Discard review</button>
          {candidate?.state === 'hooks-outside' && <button disabled={busy} onClick={() => { void execute({ action: 'skipHooks', id: job.id, revision: job.revision }); }}>Skip project hooks</button>}
          {job.status === 'failed' && candidate?.state !== 'history-required' && !findings.length && <button disabled={busy || locked} onClick={() => { void execute({ action: 'prepare', id: job.id, revision: job.revision }); }}>Retry check</button>}
          {job.status === 'commit-blocked' ? <button disabled={busy} onClick={() => { void execute({ action: 'retryCommit', id: job.id, revision: job.revision }); }}>Retry Commit</button> : <button className="commit-primary" disabled={!canCommit} onClick={() => { if (candidate) void execute({ action: 'commit', id: job.id, revision: job.revision, head: candidate.head, tree: candidate.tree, settingsHash: candidate.settingsHash }); }}>{candidate?.check && !candidate.check.command ? 'Commit without check' : 'Commit'}{pendingAssumptions > 0 ? ` · keeps ${pendingAssumptions} assumption${pendingAssumptions === 1 ? '' : 's'}` : ''}</button>}
        </>}
      </div></footer>;

  const footer=allFolders?<footer className="commit-footer"><p role="status">{group.group?.status==='done'?'All selected folders committed':group.group?.status==='committing'?'Committing folders…':group.busy?'Saving all-folder choice…':readiness?`${readiness.done} committed · ${readiness.ready} checked · ${readiness.waiting} need attention`:'Opening all-folder review…'}</p><div>
    {group.group?.status==='done'?<>{job&&track?.included&&<RealignButton key={`${job.id}:${track.id}`} source={{kind:'commit',jobId:job.id,revision:job.revision}} title={`Realign ${track.label}`} track={track} jobs={realignments} workers={workers} disabled={busy} queued={queued=>{realignChanged(queued);close();}}/>}<button onClick={close}>Done</button></>:<>
      <button disabled={busy||jobs.some(job=>group.group?.members.some(member=>member.jobId===job.id)&&(['committing','commit-blocked'].includes(job.status)||repairs.some(run=>['commit','undo'].includes(run.owner.kind)&&run.owner.id===job.id&&(!['done','discarded'].includes(run.phase)||!!run.refresh&&!run.refresh.error))))} onClick={()=>{void group.discard();}}>Discard remaining reviews</button>
      {!job&&<button disabled={busy} onClick={()=>{void group.refresh();}}>Refresh saved work</button>}
      {job?.status==='failed'&&candidate?.state!=='history-required'&&!findings.length&&<button disabled={busy||locked} onClick={()=>{void execute({action:'prepare',id:job.id,revision:job.revision});}}>Retry check</button>}
      {candidate?.state==='hooks-outside'&&job&&<button disabled={busy} onClick={()=>{void execute({action:'skipHooks',id:job.id,revision:job.revision});}}>Skip project hooks</button>}
      {job?.status==='commit-blocked'&&<button disabled={busy} onClick={()=>{void execute({action:'retryCommit',id:job.id,revision:job.revision});}}>Retry Commit for {job.project.name}</button>}
      <button className="commit-primary" disabled={busy||!!pending.current||!readiness?.canCommit} onClick={()=>{void group.commit();}}>{readiness?.done?'Commit remaining folders':'Commit all folders'}{groupAssumptions>0?` · keeps ${groupAssumptions} assumption${groupAssumptions===1?'':'s'}`:''}</button>
    </>}
  </div></footer>:individualFooter;

  if (job?.status === 'ordering' && job.orderReview) return <CommitOrder key={`${job.id}:${job.revision}`} job={job} review={job.orderReview} folderNavigation={<>{folderNavigation}{groupSummary}</>}
    busy={busy} error={error} close={close} reload={() => { void reload(); }}
    retry={pending.current ? () => { if (pending.current) void execute(pending.current.action, pending.current); } : null}
    confirm={tracks => { void execute({ action: 'order', id: job.id, revision: job.revision, tracks }); }} />;

  return <aside className="commit-sheet" aria-label="Commit review" data-commit-review data-focus-region tabIndex={-1} onKeyDown={event => {
    if(event.nativeEvent.isComposing||event.defaultPrevented)return;
    if(event.metaKey&&!event.shiftKey&&!event.altKey&&!event.ctrlKey&&(event.code==='Period'||event.key==='.')&&(undoRepair||repair)) {
      event.preventDefault();event.stopPropagation();void stopActiveRun((undoRepair??repair)!.id);return;
    }
    if (event.key === 'Escape') { event.stopPropagation(); requestClose(); }
  }}>
    <div className="commit-scroll">
    <header className="commit-heading"><div><h2>Commit</h2><p title={job?.project.name}>{job?.project.name ?? 'Opening review…'}</p></div><button aria-label="Hide commit review" onClick={requestClose}>×</button></header>
    {folderNavigation}
    {groupSummary}
    {confirmHide&&<section className="commit-refresh" role="group" aria-label="Unsaved merge editor drafts"><strong>The editor has unsaved resolution choices.</strong><p>The held merge files stay available. These editor drafts will be discarded when you hide this review.</p><div><button onClick={()=>setConfirmHide(false)}>Keep editing</button><button onClick={close}>Discard editor drafts and hide</button></div></section>}
    {job && <div className="commit-target"><span title={job.plan?.targetRef}>{job.plan?.targetRef.replace('refs/heads/', '') ?? 'Preparing…'}</span><button disabled={busy || !!undoRepair || refreshBusy || ['committing', 'commit-blocked', 'done', 'staging'].includes(job.status) || ['manual','needs-recovery'].includes(repair?.phase??'') || !!repair?.externalEditor || !!repair?.restoring} onClick={requestRefresh}>Refresh</button></div>}
    {refreshChoice && <section className="commit-refresh" role="group" aria-label="Confirm Refresh"><strong>{refreshChoice.repairId ? `Refresh stops ${refreshChoice.label}’s repair` : 'Refresh discards the reviewed fixes'}</strong><p>Saved checkpoints will replace the edits made for this commit.</p><div><button disabled={busy} onClick={() => { void confirmRefresh(); }}>Refresh now</button><button disabled={busy} onClick={() => setRefreshChoice(null)}>Wait</button></div></section>}
    {!!job?.finishedSinceOpen && <p className="commit-notice">{job.finishedSinceOpen} newer checkpoint{job.finishedSinceOpen === 1 ? '' : 's'} · Refresh to include</p>}
    {!!instructionFiles.length && <p className="commit-notice">includes instruction files · <PrivacyText text={instructionFiles.join(' · ')} /></p>}
    {!!unscannedLfs.length && <p className="commit-notice" role="status">LFS binary not scanned · <PrivacyText text={unscannedLfs.join(' · ')} /></p>}
    {error && <div className="commit-error" role="alert"><p>{error}</p><div><button disabled={busy || !pending.current} onClick={() => { if (pending.current) void execute(pending.current.action, pending.current); }}>Retry action</button><button disabled={busy} onClick={() => { void reload(); }}>Reload review</button>{pendingRepair.current && <button disabled={busy} onClick={() => { const request = pendingRepair.current; if (request) void askRepair(request.track, request.purpose, request); }}>Retry repair request</button>}{pendingRefresh.current && <button disabled={busy} onClick={() => { void confirmRefresh(pendingRefresh.current!); }}>Retry confirmed Refresh</button>}</div></div>}
    {!job ? <p className="commit-empty">{busy ? 'Loading saved checkpoints…' : allFolders?'No saved work selected for this folder.':'No saved work is available.'}</p> : <>
      <PathChoices key={job.id} job={job} comparisons={comparisons} connected={connected} realignments={realignments} queued={realignChanged} workers={workers} effects={effectsForJob} disabled={busy||locked||dirty} execute={action=>{void execute(action);}} showTrack={id=>{setTrackId(id);setTab('changes');}}/>
      <UndoMerge job={job} run={undoRepair} flows={flows} secrets={secretInputs} changed={reload} dirtyChanged={setMergeDirty}/>
      <UndoReview key={job.id} job={job} focus={undoFocus?.job===job.id?undoFocus:null}/>
      <div className="commit-tracks" role="group" aria-label="Tracks to include">{job.tracks.map(item => {
        const state = job.plan?.rows.find(row => row.track === item.id)?.state;
        const impact = job.exclusions.find(value => value.track === item.id);
        const blockedBy = job.exclusions.filter(value => value.omitted && value.dependents.includes(item.id));
        const label = (id: string) => job.tracks.find(value => value.id === id)?.label ?? '?';
        const retired=job.pathState!==null&&Object.hasOwn(job.pathState.retired,item.id);
        const notice = retired?'Choose this path above to include it.':impact?.dependents.length ? `Leaving ${item.label} out also leaves ${impact.dependents.map(label).join(', ')} out (inherits ${item.label})`
          : blockedBy.length ? `Left out · inherits ${blockedBy.map(value => label(value.track)).join(', ')}. Include the source first to select this track.` : '';
        return <div key={item.id} className="commit-track-group"><div className={item.id === trackId ? 'commit-track selected' : 'commit-track'}>
          <input type="checkbox" aria-label={`Include Worker ${item.label}`} aria-describedby={notice ? `commit-exclusion-${item.id}` : undefined} checked={item.included} disabled={busy || locked || !item.checkpoint || retired || blockedBy.length > 0} onChange={event => { void execute({ action: 'select', id: job.id, revision: job.revision, track: item.id, included: event.target.checked }); }} />
          <button aria-pressed={item.id === trackId} onClick={() => setTrackId(item.id)}><b>{item.label}</b><span>{item.title}<small>{!item.checkpoint ? 'No saved checkpoint' : state === 'no-changes' ? 'No changes' : state === 'conflict' ? 'Conflict' : `Saved ${item.label}@${item.checkpointCount}`}</small></span></button>
        </div>{notice && <p className="commit-exclusion" id={`commit-exclusion-${item.id}`}>{notice}</p>}</div>;
      })}</div>
      {job.error && <p className="commit-notice" role="status">{job.error}</p>}
      {(job.settingsChanged || job.status === 'failed' || tab === 'check' && candidate?.check) && <div className="commit-notice check-settings" role="status"><span>{job.settingsChanged ? 'Checked with previous settings' : 'Check settings are fixed for this review'}</span><button disabled={busy || locked || dirty || !job.plan?.tree} onClick={() => { void execute({ action: 'recheck', id: job.id, revision: job.revision }); }}>Re-check</button></div>}
      {candidate?.state === 'history-required' && <section className="commit-history" aria-label="Check needs Git history"><strong>This check needs Git history</strong><p>Enabling history lets Check and project hooks read full history and tags, including removed files and secrets.</p><div><button disabled={busy || locked || dirty} onClick={() => { void enableHistory(); }}>{pendingHistory.current ? 'Retry enabling history' : 'Enable history for checks'}</button><button disabled={busy || locked || !!pendingHistory.current} onClick={() => { void execute({ action: 'treatCheckAsFailure', id: job.id, revision: job.revision }); }}>Treat as failure</button></div></section>}
      {!!secretFindings.length && <div className="commit-secrets" aria-label="Secret findings"><strong>Secret found in the diff · {secretFindings.every(finding=>undoFinding(finding.id))?'Undo commit':`Worker ${job.tracks.find(track => track.id === (candidate?.blockedCommit?.track ?? candidate?.failedTrack))?.label ?? '?'}`}</strong><p>Keep includes this finding in commit history.</p>{secretFindings.map(finding => <div key={finding.id}><button onClick={() => { if(undoFinding(finding.id)){setUndoFocus(current=>({job:job.id,path:finding.path,sequence:(current?.sequence??0)+1}));return;} setTrackId(candidate?.blockedCommit?.track ?? candidate?.failedTrack ?? trackId); setFile(finding.path); setTab(finding.path === 'commit-message.txt' ? 'message' : 'changes'); }}>{finding.path === 'commit-message.txt' ? 'Commit message' : finding.path}:{finding.line}<small>{finding.rule === 'github-pat' ? 'GitHub token' : finding.rule.replaceAll('-', ' ')}</small></button><button disabled={busy || locked || undoFinding(finding.id) || !(candidate?.blockedCommit?.track ?? candidate?.failedTrack)} onClick={() => { const owner = candidate?.blockedCommit?.track ?? candidate?.failedTrack; if (owner) void askRepair(owner, { kind: 'removeSecret', finding: finding.id }); }}>Ask to remove</button><button disabled={busy || locked} onClick={() => { void execute({ action: 'keepSecret', id: job.id, revision: job.revision, finding: finding.id }); }}>Keep this finding</button></div>)}</div>}
      {!!binaryReviews.length && <section className="commit-secrets" aria-label="Binary files need review">
        <strong>Binary files need review</strong>
        <p>Checked for supplied secret values. Pattern detection does not cover these binary contents.</p>
        {binaryReviews.map(finding => <div key={finding.id}>
          <PrivacyText text={finding.path} />
          <button disabled={busy || locked} onClick={() => { void execute({ action: 'keepSecret', id: job.id, revision: job.revision, finding: finding.id }); }}>Keep binary file</button>
        </div>)}
      </section>}
      {!!keptBinaries && <p className="commit-notice">{keptBinaries} binary file{keptBinaries === 1 ? '' : 's'} kept after review.</p>}
      {!!keptSecrets && <p className="commit-notice">{keptSecrets} secret finding{keptSecrets === 1 ? '' : 's'} kept in this commit.</p>}
      <RepairAttribution job={job}/>
      {latestRepair && repair?.phase!=='manual' && <RepairPanel waitingForMemory={!!hostMemory?.waiting.some(entry => entry.owner === latestRepair.id)} secretInputs={secretInputs} preview={flows.find(flow => flow.repairRunId === latestRepair.id)?.preview} key={latestRepair.id} run={latestRepair} label={job.tracks.find(item => item.id === latestRepair.track)?.label ?? '?'} changed={reload} />}
      {repair?.phase==='manual'?<Suspense fallback={<p className="commit-empty">Opening conflict editor…</p>}><ManualMerge run={repair} label={job.tracks.find(item=>item.id===repair.track)?.label??'?'} targetLabel={[...new Set(job.plan?.rows.find(row=>row.track===repair.track)?.conflicts.flatMap(conflict=>conflict.withTracks)??[])].map(id=>job.tracks.find(item=>item.id===id)?.label??'?').join(' + ')||'target'} changed={reload} dirtyChanged={setMergeDirty}/></Suspense>:undoRepair?.phase==='manual'?null:<>
      <div className="commit-tabs" role="tablist" aria-label="Review details">{(['changes', 'message', 'check'] as const).map(value => <button key={value} role="tab" aria-selected={tab === value} onClick={() => setTab(value)}>{value === 'check' ? 'Check' : value === 'message' ? 'Message' : 'Changes'}</button>)}</div>
      <div className="commit-details">
      {job.assumptionsPending > 0 && <p className="commit-notice" role="status">Waiting for assumption reading · {job.assumptionsPending} message{job.assumptionsPending === 1 ? '' : 's'}</p>}
      {job.assumptionsFailed > 0 && <div className="commit-notice" role="status">Couldn’t read assumptions <button disabled={busy || locked} onClick={() => { void execute({ action: 'retryAssumptions', id: job.id, revision: job.revision }); }}>Retry reading</button></div>}
      <Assumptions job={job} disabled={busy || locked || dirty || job.assumptionsPending > 0 || job.assumptionsFailed > 0 || job.status !== 'checked'}
        keep={assumption => { void execute({ action: 'keepAssumption', id: job.id, revision: job.revision, assumption: assumption.id }); }}
        change={(assumption, text) => { void askRepair(assumption.trackId, { kind: 'assumption', id: assumption.id, revision: assumption.revision, text }); }} />
        {tab === 'changes' && <><div className="commit-files" aria-label="Changed files">{changes.map(change => <button key={change.path} aria-pressed={change.path === file} onClick={() => setFile(change.path)}><span>{change.before ? change.after ? 'M' : 'D' : 'A'}</span>{change.path}</button>)}</div>
          {row?.conflicts.map(conflict => <p className="commit-notice" key={conflict.path}>Conflict · {conflict.path}</p>)}
          {row?.state === 'conflict' && track && <div className="commit-conflict-actions">{repair&&['queued','preparing','running','stopping','captured','applying','needs-recovery'].includes(repair.phase)?<button onClick={()=>setTab('changes')}>Open diff</button>:<><button disabled={busy||!!repair?.remaining} onClick={()=>{void resolveManually();}}>{pendingManual.current?'Retry opening editor':'Let me resolve'}</button><button disabled={busy||!!repair?.remaining||!!repair?.externalEditor} onClick={()=>{void mergeChoice('resume');}}>Let {track.label} try again</button><button className="commit-leave-out" disabled={busy||!!repair?.externalEditor} onClick={()=>{void mergeChoice('discard');}}>Leave {track.label} out</button></>}</div>}
          {!changes.length ? <p className="commit-empty">{row?.state === 'conflict' ? 'Exclude this track or resolve its conflict before continuing.' : 'No file changes in this row.'}</p> : diffError ? <p className="commit-error" role="alert">{diffError}</p> : diff ? <>{diff.redacted && <p className="commit-notice">Sensitive text is hidden in this preview.</p>}<Suspense fallback={<p className="commit-empty">Loading diff…</p>}><DiffView value={diff} select={selectedLines} /></Suspense></> : <p className="commit-empty">Loading diff…</p>}
        </>}
        {tab === 'message' && track && <div className="commit-message"><label htmlFor="commit-message-body">Message for Worker {track.label}</label><textarea id="commit-message-body" value={draft?.text ?? job.messages[track.id] ?? track.title} disabled={busy || locked || !track.included} onChange={event => setDrafts(current => ({ ...current, [track.id]: { text: event.target.value, revision: draft?.revision ?? job.revision } }))} />
          {draft && draft.revision !== job.revision && <p className="commit-notice">The review changed while editing. Your draft has been kept.</p>}
          <div><button disabled={busy || locked || !track.included || !draft?.text.trim() || draft.revision !== job.revision} onClick={() => { if (draft) void execute({ action: 'editMessage', id: job.id, revision: draft.revision, track: track.id, message: draft.text }).then(result => { if (result) setDrafts(current => { const next = { ...current }; delete next[track.id]; return next; }); }); }}>Save message and check</button>{draft && <button onClick={() => setDrafts(current => { const next = { ...current }; delete next[track.id]; return next; })}>Discard message draft</button>}</div>
          {commit && <details><summary>Final message after hooks</summary><pre>{commit.message}</pre></details>}<p>The Intercat reference is appended to the message.</p>
        </div>}
        {tab === 'check' && <div className="commit-check">{job.status === 'failed' && ['check-failed', 'check-mutated'].includes(candidate?.state ?? '') && <RepairOwnerChoices job={job} disabled={busy || locked || !!pendingRepair.current} choose={id=>{void askRepair(id,{kind:'check'});}}/>}<strong>{job.status === 'building' || job.status === 'staging' ? 'Checking…' : candidate?.check ? candidate.check.command ? candidate.check.code === 0 ? 'Check passed' : `Check failed · exit ${candidate.check.code ?? 'unknown'}` : 'No check configured' : 'Check has not run'}</strong>{candidate?.check?.command && <code>{candidate.check.command}</code>}{candidate?.hooksSkipped && <p>Project hooks were skipped.</p>}{!!candidate?.dependencyRuns.length && <p>Dependencies prepared · {candidate.dependencyRuns.reduce((total, run) => total + run.downloads, 0)} downloaded</p>}<pre>{[candidate?.check?.stdout, candidate?.check?.stderr].filter(Boolean).join('\n') || candidate?.reason || 'No output.'}</pre></div>}
      </div>
      </>}
      {!!effectsForJob.length && <details className="commit-effects"><summary>Effects outside Git · {effectsForJob.length}</summary><Effects effects={effectsForJob} /></details>}

    </>}
    </div>
    {footer}
  </aside>;
}
