import { lazy, Suspense, useEffect, useRef, useState } from 'react';
import type { Effect, OutputFlow } from '../bridge';
import { Effects } from '../desk/Effects';
import type { Diff, IntegrationAction, IntegrationJob, IntegrationRequest } from './types';
import { Assumptions } from './Assumptions';
import { CommitOrder } from './CommitOrder';
import { RepairPanel } from './RepairPanel';
import type { RepairPurpose, RepairRequest, RepairRun, RepairRefreshRequest } from './repair';
import { bindContext, type CommitContext, type DiffSelection } from './context';
import './commit.css';

const DiffView = lazy(() => import('./DiffView'));
type Draft = { text: string; revision: number };
export default function CommitSheet({ initialRequest, job, effects, repairs, flows, secretInputs, update, close, selectContext }: { initialRequest?: IntegrationRequest; secretInputs: import('../bridge').SecretInputState[]; job: IntegrationJob | null; effects: Effect[]; repairs: RepairRun[]; flows: OutputFlow[]; update: (job: IntegrationJob) => Promise<void>; close: () => void; selectContext: (context: CommitContext) => void }) {
  const [busy, setBusy] = useState(true);
  const [error, setError] = useState('');
  const [trackId, setTrackId] = useState('');
  const [file, setFile] = useState('');
  const [tab, setTab] = useState<'changes' | 'message' | 'check'>('changes');
  const [diff, setDiff] = useState<Diff | null>(null);
  const [diffError, setDiffError] = useState('');
  const diffBinding = useRef<{ job: IntegrationJob; track: string; change: import('./types').Change } | null>(null);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [refreshChoice, setRefreshChoice] = useState<{ id: string; revision: number; repairId: string | null; label: string } | null>(null);
  const pending = useRef<IntegrationRequest | null>(null);
  const opening = useRef(false);
  const pendingRepair = useRef<RepairRequest | null>(null);
  const pendingRefresh = useRef<RepairRefreshRequest | null>(null);
  const repair = repairs.find(run => run.jobId === job?.id && (!['done', 'discarded'].includes(run.phase) || (run.refresh && !run.refresh.error)));
  const latestRepair = repair ?? (Object.keys(job?.contributions ?? {}).length ? repairs.find(run => run.jobId === job?.id && run.phase === 'done') : undefined);
  const locked = !!repair || !!job && ['committing', 'commit-blocked', 'done'].includes(job.status);
  const refreshBusy = !!repair?.refresh && !repair.refresh.error;

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
        await update(result);
        if (['open', 'select', 'refresh', 'order'].includes(request.action.action) && result.status === 'open' && result.plan?.tree) {
          request = { opId: await window.intercat.createOperationId(), action: { action: 'prepare', id: result.id, revision: result.revision } };
        } else break;
      }
      pending.current = null;
      return result;
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The action could not be confirmed.'); return null; }
    finally { setBusy(false); }
  }
  useEffect(() => {
    if (!opening.current) { opening.current = true; void execute({ action: 'open', tracks: null }, initialRequest); }
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
    if (job && track && file) void window.intercat.getIntegrationDiff({ id: job.id, revision: job.revision, track: track.id, path: file }).then(value => { if (!cancelled) { setDiff(value); if (change) diffBinding.current = { job, track: track.id, change }; } }).catch(failure => { if (!cancelled) setDiffError(failure instanceof Error ? failure.message : 'The diff is unavailable.'); });
    return () => { cancelled = true; };
  }, [job?.id, job?.revision, commit?.sha, trackId, file]);

  async function reload() {
    try { const status = await window.intercat.getStatus(); if (status.integration) await update(status.integration); pending.current = null; setError(''); }
    catch { setError('The current review could not be loaded.'); }
  }
  async function askRepair(owner: string, purpose: RepairPurpose, retry?: RepairRequest) {
    if (!job) return;
    setBusy(true); setError('');
    try {
      const request = retry ?? { opId: await window.intercat.createOperationId(), jobId: job.id, revision: job.revision, track: owner, purpose };
      pendingRepair.current = request;
      const response = await window.intercat.startRepair(request);
      if (response.status === 'rejected') { pendingRepair.current = null; setError(response.reason); return; }
      pendingRepair.current = null;
      await reload();
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The repair request could not be confirmed.'); }
    finally { setBusy(false); }
  }
  const candidate = job?.candidate;
  function selectedLines(range: DiffSelection) {
    const binding = diffBinding.current;
    if (!binding || !['checked', 'failed'].includes(binding.job.status)) return;
    const context = bindContext(binding.job, binding.track, binding.change, range);
    if (context) selectContext(context);
  }
  const repairOwners = job?.tracks.filter(item => item.included && candidate?.commits.some(commit => commit.track === item.id)) ?? [];
  const repairOwner = repairOwners.find(item => item.id === trackId) ?? repairOwners[0];
  const findings = candidate?.secretScans.flatMap(scan => scan.findings).filter(finding => !job?.keptSecrets.includes(finding.id)) ?? [];
  const dirty = Object.keys(drafts).length > 0;
  const draft = track ? drafts[track.id] : undefined;
  const pendingAssumptions = job?.assumptions.filter(assumption => assumption.included && assumption.state === 'open').length ?? 0;
  const canCommit = !job?.assumptionsPending && !job?.assumptionsFailed && !job?.assumptionsChanged && !busy && !repair && !pending.current && !dirty && job?.status === 'checked' && candidate && job.tracks.some(track => track.included && track.checkpoint);
  const effectsForJob = effects.filter(effect => job?.tracks.some(track => track.id === effect.trackId));

  if (job?.status === 'ordering' && job.orderReview) return <CommitOrder key={`${job.id}:${job.revision}`} job={job} review={job.orderReview}
    busy={busy} error={error} close={close} reload={() => { void reload(); }}
    retry={pending.current ? () => { if (pending.current) void execute(pending.current.action, pending.current); } : null}
    confirm={tracks => { void execute({ action: 'order', id: job.id, revision: job.revision, tracks }); }} />;

  return <aside className="commit-sheet" aria-label="Commit review" data-focus-region tabIndex={-1} onKeyDown={event => { if (event.key === 'Escape' && !event.nativeEvent.isComposing) { event.stopPropagation(); close(); } }}>
    <header className="commit-heading"><div><h2>Commit</h2><p>{job?.project.name ?? 'Opening review…'}</p></div><button aria-label="Hide commit review" onClick={close}>×</button></header>
    {job && <div className="commit-target"><span title={job.plan?.targetRef}>{job.plan?.targetRef.replace('refs/heads/', '') ?? 'Preparing…'}</span><button disabled={busy || refreshBusy || ['committing', 'commit-blocked', 'done', 'staging'].includes(job.status) || repair?.phase === 'needs-recovery' || !!repair?.restoring} onClick={requestRefresh}>Refresh</button></div>}
    {refreshChoice && <section className="commit-refresh" role="group" aria-label="Confirm Refresh"><strong>{refreshChoice.repairId ? `Refresh stops ${refreshChoice.label}’s repair` : 'Refresh discards the reviewed fixes'}</strong><p>Saved checkpoints will replace the edits made for this commit.</p><div><button disabled={busy} onClick={() => { void confirmRefresh(); }}>Refresh now</button><button disabled={busy} onClick={() => setRefreshChoice(null)}>Wait</button></div></section>}
    {!!job?.finishedSinceOpen && <p className="commit-notice">{job.finishedSinceOpen} newer checkpoint{job.finishedSinceOpen === 1 ? '' : 's'} · Refresh to include</p>}
    {error && <div className="commit-error" role="alert"><p>{error}</p><div><button disabled={busy || !pending.current} onClick={() => { if (pending.current) void execute(pending.current.action, pending.current); }}>Retry action</button><button disabled={busy} onClick={() => { void reload(); }}>Reload review</button>{pendingRepair.current && <button disabled={busy} onClick={() => { const request = pendingRepair.current; if (request) void askRepair(request.track, request.purpose, request); }}>Retry repair request</button>}{pendingRefresh.current && <button disabled={busy} onClick={() => { void confirmRefresh(pendingRefresh.current!); }}>Retry confirmed Refresh</button>}</div></div>}
    {!job ? <p className="commit-empty">{busy ? 'Loading saved checkpoints…' : 'No saved work is available.'}</p> : <>
      <div className="commit-tracks" role="group" aria-label="Tracks to include">{job.tracks.map(item => {
        const state = job.plan?.rows.find(row => row.track === item.id)?.state;
        return <div key={item.id} className={item.id === trackId ? 'commit-track selected' : 'commit-track'}>
          <input type="checkbox" aria-label={`Include Worker ${item.label}`} checked={item.included} disabled={busy || locked || !item.checkpoint} onChange={event => { void execute({ action: 'select', id: job.id, revision: job.revision, track: item.id, included: event.target.checked }); }} />
          <button aria-pressed={item.id === trackId} onClick={() => setTrackId(item.id)}><b>{item.label}</b><span>{item.title}<small>{!item.checkpoint ? 'No saved checkpoint' : state === 'no-changes' ? 'No changes' : state === 'conflict' ? 'Conflict' : `Saved ${item.label}@${item.checkpointCount}`}</small></span></button>
        </div>;
      })}</div>
      {job.error && <p className="commit-notice" role="status">{job.error}</p>}
      {(job.settingsChanged || job.status === 'failed' || tab === 'check' && candidate?.check) && <div className="commit-notice check-settings" role="status"><span>{job.settingsChanged ? 'Checked with previous settings' : 'Check settings are fixed for this review'}</span><button disabled={busy || locked || dirty || !job.plan?.tree} onClick={() => { void execute({ action: 'recheck', id: job.id, revision: job.revision }); }}>Re-check</button></div>}
      {!!findings.length && <div className="commit-secrets" aria-label="Secret findings"><strong>Secret found in the diff · Worker {job.tracks.find(track => track.id === candidate?.blockedCommit?.track)?.label ?? '?'}</strong><p>Keep includes this finding in commit history.</p>{findings.map(finding => <div key={finding.id}><button onClick={() => { setTrackId(candidate?.blockedCommit?.track ?? trackId); setFile(finding.path); setTab(finding.path === 'commit-message.txt' ? 'message' : 'changes'); }}>{finding.path === 'commit-message.txt' ? 'Commit message' : finding.path}:{finding.line}<small>{finding.rule === 'github-pat' ? 'GitHub token' : finding.rule.replaceAll('-', ' ')}</small></button><button disabled={busy || locked || !candidate?.blockedCommit} onClick={() => { if (candidate?.blockedCommit) void askRepair(candidate.blockedCommit.track, { kind: 'removeSecret', finding: finding.id }); }}>Ask to remove</button><button disabled={busy || locked} onClick={() => { void execute({ action: 'keepSecret', id: job.id, revision: job.revision, finding: finding.id }); }}>Keep this finding</button></div>)}</div>}
      {!!job.keptSecrets.length && <p className="commit-notice">{job.keptSecrets.length} secret finding{job.keptSecrets.length === 1 ? '' : 's'} kept in this commit.</p>}
      {latestRepair && <RepairPanel secretInputs={secretInputs} preview={flows.find(flow => flow.repairRunId === latestRepair.id)?.preview} key={latestRepair.id} run={latestRepair} label={job.tracks.find(item => item.id === latestRepair.track)?.label ?? '?'} changed={reload} />}
      <div className="commit-tabs" role="tablist" aria-label="Review details">{(['changes', 'message', 'check'] as const).map(value => <button key={value} role="tab" aria-selected={tab === value} onClick={() => setTab(value)}>{value === 'check' ? 'Check' : value === 'message' ? 'Message' : 'Changes'}</button>)}</div>
      <div className="commit-details">
      {job.assumptionsPending > 0 && <p className="commit-notice" role="status">Waiting for assumption reading · {job.assumptionsPending} message{job.assumptionsPending === 1 ? '' : 's'}</p>}
      {job.assumptionsFailed > 0 && <div className="commit-notice" role="status">Couldn’t read assumptions <button disabled={busy || locked} onClick={() => { void execute({ action: 'retryAssumptions', id: job.id, revision: job.revision }); }}>Retry reading</button></div>}
      <Assumptions job={job} disabled={busy || locked || dirty || job.assumptionsPending > 0 || job.assumptionsFailed > 0 || job.status !== 'checked'}
        keep={assumption => { void execute({ action: 'keepAssumption', id: job.id, revision: job.revision, assumption: assumption.id }); }}
        change={(assumption, text) => { void askRepair(assumption.trackId, { kind: 'assumption', id: assumption.id, revision: assumption.revision, text }); }} />
        {tab === 'changes' && <><div className="commit-files" aria-label="Changed files">{changes.map(change => <button key={change.path} aria-pressed={change.path === file} onClick={() => setFile(change.path)}><span>{change.before ? change.after ? 'M' : 'D' : 'A'}</span>{change.path}</button>)}</div>
          {row?.conflicts.map(conflict => <p className="commit-notice" key={conflict.path}>Conflict · {conflict.path}</p>)}
          {!changes.length ? <p className="commit-empty">{row?.state === 'conflict' ? 'Exclude this track or resolve its conflict before continuing.' : 'No file changes in this row.'}</p> : diffError ? <p className="commit-error" role="alert">{diffError}</p> : diff ? <>{diff.redacted && <p className="commit-notice">Sensitive text is hidden in this preview.</p>}<Suspense fallback={<p className="commit-empty">Loading diff…</p>}><DiffView value={diff} select={selectedLines} /></Suspense></> : <p className="commit-empty">Loading diff…</p>}
        </>}
        {tab === 'message' && track && <div className="commit-message"><label htmlFor="commit-message-body">Message for Worker {track.label}</label><textarea id="commit-message-body" value={draft?.text ?? job.messages[track.id] ?? track.title} disabled={busy || locked || !track.included} onChange={event => setDrafts(current => ({ ...current, [track.id]: { text: event.target.value, revision: draft?.revision ?? job.revision } }))} />
          {draft && draft.revision !== job.revision && <p className="commit-notice">The review changed while editing. Your draft has been kept.</p>}
          <div><button disabled={busy || locked || !track.included || !draft?.text.trim() || draft.revision !== job.revision} onClick={() => { if (draft) void execute({ action: 'editMessage', id: job.id, revision: draft.revision, track: track.id, message: draft.text }).then(result => { if (result) setDrafts(current => { const next = { ...current }; delete next[track.id]; return next; }); }); }}>Save message and check</button>{draft && <button onClick={() => setDrafts(current => { const next = { ...current }; delete next[track.id]; return next; })}>Discard message draft</button>}</div>
          {commit && <details><summary>Final message after hooks</summary><pre>{commit.message}</pre></details>}<p>The Intercat reference is appended to the message.</p>
        </div>}
        {tab === 'check' && <div className="commit-check">{job.status === 'failed' && ['check-failed', 'check-mutated'].includes(candidate?.state ?? '') && repairOwner && <button className="commit-fix" disabled={busy || locked || !!pendingRepair.current} onClick={() => { void askRepair(repairOwner.id, { kind: 'check' }); }}>Ask Worker {repairOwner.label} to fix</button>}<strong>{job.status === 'building' || job.status === 'staging' ? 'Checking…' : candidate?.check ? candidate.check.command ? candidate.check.code === 0 ? 'Check passed' : `Check failed · exit ${candidate.check.code ?? 'unknown'}` : 'No check configured' : 'Check has not run'}</strong>{candidate?.check?.command && <code>{candidate.check.command}</code>}{candidate?.hooksSkipped && <p>Project hooks were skipped.</p>}{!!candidate?.dependencyRuns.length && <p>Dependencies prepared · {candidate.dependencyRuns.reduce((total, run) => total + run.downloads, 0)} downloaded</p>}<pre>{[candidate?.check?.stdout, candidate?.check?.stderr].filter(Boolean).join('\n') || candidate?.reason || 'No output.'}</pre></div>}
      </div>
      {!!effectsForJob.length && <details className="commit-effects"><summary>Effects outside Git · {effectsForJob.length}</summary><Effects effects={effectsForJob} /></details>}
      <footer className="commit-footer"><p role="status">{busy ? 'Saving review…' : job.status === 'done' ? `Committed · ${candidate?.head.slice(0, 8)}` : job.status === 'committing' ? 'Committing… · wait' : job.status === 'building' || job.status === 'staging' ? 'Building candidate and checking…' : dirty ? 'Save or discard message drafts before committing.' : job.status === 'checked' ? candidate?.check?.command ? 'Checked and ready' : 'Ready · no check configured' : 'Review needs attention'}</p><div>
        {job.status === 'done' ? <>{job.project.baseBranch === null && <><span>no base branch yet</span><button disabled title="no base branch yet">Open PR</button></>}<button onClick={close}>Done</button></> : <><button disabled={busy || locked} onClick={() => { void execute({ action: 'discard', id: job.id, revision: job.revision }).then(result => { if (result) close(); }); }}>Discard review</button>
          {candidate?.state === 'hooks-outside' && <button disabled={busy} onClick={() => { void execute({ action: 'skipHooks', id: job.id, revision: job.revision }); }}>Skip project hooks</button>}
          {job.status === 'failed' && !findings.length && <button disabled={busy || locked} onClick={() => { void execute({ action: 'prepare', id: job.id, revision: job.revision }); }}>Retry check</button>}
          {job.status === 'commit-blocked' ? <button disabled={busy} onClick={() => { void execute({ action: 'retryCommit', id: job.id, revision: job.revision }); }}>Retry Commit</button> : <button className="commit-primary" disabled={!canCommit} onClick={() => { if (candidate) void execute({ action: 'commit', id: job.id, revision: job.revision, head: candidate.head, tree: candidate.tree, settingsHash: candidate.settingsHash }); }}>{candidate?.check && !candidate.check.command ? 'Commit without check' : 'Commit'}{pendingAssumptions > 0 ? ` · keeps ${pendingAssumptions} assumption${pendingAssumptions === 1 ? '' : 's'}` : ''}</button>}
        </>}
      </div></footer>
    </>}
  </aside>;
}
