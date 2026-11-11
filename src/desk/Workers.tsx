import { queueLabel } from '../scheduling';
import { useState } from 'react';
import type { UnloggedControl, Approval, ApprovalResponse, LiveTrack, WorkerState, Prompt, PromptResponse, SecretEntry, SecretInputState, PendingInheritance } from '../bridge';
import { QuestionCard } from './QuestionCard';
import { ApprovalCard } from './ApprovalCard';
import { approvalContext } from './ApprovalDrafts';
import { Effects } from './Effects';
import type { Effect, OutputFlow } from '../bridge';
import { ProgressOutput } from './ProgressOutput';
import { MarkdownMessage } from '../markdown/MarkdownMessage';
import { ReferenceScreening } from './ReferenceScreening';
import type { AttentionCard } from './NeedsYou';
import { PairedInstructionEffort, WorkerModel } from './WorkerModel';
import { canPrepareFresh, freshPhaseText, type ConversationContinuation, type FreshJob } from '../fresh/types';
import { branchContext } from '../timeline/branchPointLayout';


import { realignHoldsWork,type RealignJob,type PendingRealignment } from '../commit/realign';
type Props = { freshJobs:FreshJob[]; continuations:ConversationContinuation[]; closedWriters:ReadonlySet<string>; connected:boolean; openFresh:(worker:WorkerState)=>void; openTrack:(id:string)=>void; retiredTracks:ReadonlySet<string>; realignments:RealignJob[]; pendingRealignments:PendingRealignment[]; pendingInheritance?: PendingInheritance[]; queued?: ReadonlyMap<string, number>; unloggedControls?: UnloggedControl[]; secretInputs?: SecretInputState[]; workers: WorkerState[]; tracks: LiveTrack[]; flows?: OutputFlow[]; effects?: Effect[]; start: (id: string) => void; stop: (id: string) => void; canDropFiles: boolean; dropFiles: (files: File[], trackId: string) => void };
type CardProps = Pick<Props, 'workers' | 'tracks'> & {
  pendingInheritance: PendingInheritance[];
  finishInheritance: (pending: PendingInheritance) => Promise<void>;
  provideSecret: (worker: WorkerState, prompt: Prompt, entry: SecretEntry) => Promise<void>;
  answer: (opId: string, worker: WorkerState, card: Approval, response: ApprovalResponse) => Promise<void>;
  respond: (worker: WorkerState, prompt: Prompt, response: PromptResponse) => Promise<void>;
};

function InheritanceRecovery({ pending, worker, label, finish }: { pending: PendingInheritance; worker: WorkerState; label: string; finish: CardProps['finishInheritance'] }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const ready = !worker.activeTurn && !worker.checkpointPending && ['held','ready'].includes(worker.phase);
  return <div className="inheritance-recovery">
    <strong>File update for Worker {label}</strong>
    <p>Finish applying the saved files. No request will be sent.</p>
    {pending.sourceState === 'cut' && <p>The request stays taken back.</p>}
    {worker.detail && worker.phase === 'held' && <p>{worker.detail}</p>}
    {error && <p role="alert">{error}</p>}
    <div className="inheritance-actions"><button disabled={busy || !ready} onClick={() => { setBusy(true); setError(''); void finish(pending).catch(error => setError((error instanceof Error ? error.message : String(error)).replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/, ''))).finally(() => setBusy(false)); }}>{busy || worker.phase === 'finishing-inheritance' ? 'Finishing…' : 'Finish file update'}</button></div>
  </div>;
}

export function workerCards({ workers, tracks, pendingInheritance, finishInheritance, provideSecret, answer, respond }: CardProps): AttentionCard[] {
  return [
    ...pendingInheritance.flatMap(pending => {
      const worker = workers.find(worker => worker.trackId === pending.trackId);
      if (!worker || worker.phase === 'starting') return [];
      const label = tracks.find(track => track.id === pending.trackId)?.label ?? '';
      return [{ id: `inheritance-${pending.operationId}`, version: `${pending.state}:${pending.sourceState}`, kind: 'stopped' as const, destination: { trackId: pending.trackId, label }, title: `Saved files for Worker ${label}`, content: <InheritanceRecovery key={pending.operationId} pending={pending} worker={worker} label={label} finish={finishInheritance} /> }];
    }),
    ...workers.flatMap(worker => worker.prompts.map(prompt => ({
      id: `prompt-${worker.trackId}-${prompt.request.userInputId}`,
      version: prompt.revision,
      kind: 'stopped' as const,
      destination: { trackId: worker.trackId, label: tracks.find(track => track.id === worker.trackId)?.label ?? '' },
      title: `Question for Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`,
      content: <QuestionCard key={`${prompt.request.userInputId}:${prompt.revision}`} provideSecret={(prompt, entry) => provideSecret(worker, prompt, entry)} stopping={['stopping', 'reconnecting', 'lost', 'needs-recovery'].includes(worker.phase)} prompt={prompt} label={`Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`} respond={(prompt, response) => respond(worker, prompt, response)} />,
    }))),
    ...workers.flatMap(worker => worker.approvals.map(card => ({
      id: `approval-${worker.trackId}-${card.id}`,
      version: JSON.stringify([card.requirement, card.subject, card.reason, card.replyMode]),
      kind: 'stopped' as const,
      destination: { trackId: worker.trackId, label: tracks.find(track => track.id === worker.trackId)?.label ?? '' },
      title: `Approval for Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`,
      content: <ApprovalCard key={JSON.stringify(card.requirement)} stopping={['stopping', 'reconnecting', 'lost', 'needs-recovery'].includes(worker.phase)} detail={worker.detail} label={`Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`} card={card} draftContext={approvalContext({ kind: 'work' }, worker.trackId, worker.sessionId, `Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`, card)} answer={(opId, card, response) => answer(opId, worker, card, response)} />,
    }))),
  ];
}

export function Workers({ freshJobs, continuations, closedWriters, connected, openFresh, openTrack, retiredTracks, realignments, pendingRealignments, pendingInheritance = [], queued, unloggedControls = [], workers, tracks, flows = [], effects = [], secretInputs = [], start, stop, canDropFiles, dropFiles }: Props) {
  const [dropTrack, setDropTrack] = useState<string | null>(null);
  return <div className="workers">
    {workers.map(worker => { const track = tracks.find(track => track.id === worker.trackId); const label = `Worker ${track?.label ?? ''}`; const fresh = freshJobs.findLast(job => job.request.trackId === worker.trackId && job.request.sessionId === worker.sessionId && job.phase !== 'cancelled'); const closed = closedWriters.has(worker.trackId); const lineage = continuations.find(link => link.successor === worker.trackId); const successor = continuations.find(link => link.source === worker.trackId); const acceptsFiles = canDropFiles && !!track && !closed && !fresh && !retiredTracks.has(worker.trackId); return <section key={worker.trackId} data-worker-card={worker.trackId} className={dropTrack === worker.trackId && acceptsFiles ? 'file-drop-active' : undefined}
      onDragOver={event => {
        if (!event.dataTransfer.types.includes('Files')) return;
        event.preventDefault(); event.stopPropagation(); event.dataTransfer.dropEffect = acceptsFiles ? 'copy' : 'none';
        setDropTrack(acceptsFiles ? worker.trackId : null);
      }}
      onDragLeave={event => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) setDropTrack(null); }}
      onDrop={event => { if (!event.dataTransfer.types.includes('Files')) return; event.preventDefault(); event.stopPropagation(); setDropTrack(null); if (acceptsFiles) dropFiles(Array.from(event.dataTransfer.files), worker.trackId); }}>
      {dropTrack === worker.trackId && acceptsFiles && <div className="card-file-drop"><span>Drop to attach · next words to {label}</span></div>}
      <div className="worker-controls"><strong>{label}</strong><span>{queued?.has(worker.trackId) && !worker.activeTurn ? queueLabel(queued.get(worker.trackId)!) : ['stopped', 'after-connection-loss'].includes(worker.phase) && worker.detail ? worker.detail : worker.phase === 'checkpoint-blocked' ? 'checkpoint needs you' : worker.phase === 'waiting-memory' ? 'queued · waiting for memory' : worker.phase === 'sleeping' ? 'ready' : worker.phase === 'reviewing-fix' ? 'reviewing the fix' : worker.phase === 'review-conflict' ? 'waiting for your review decision' : worker.phase === 'finishing-inheritance' ? 'finishing file update' : worker.phase === 'stopping-processes' ? 'stopping processes' : worker.phase}</span>{!['working','starting'].includes(worker.phase) && unloggedControls.some(control => control.paused !== false && (!control.trackId || control.trackId === worker.trackId)) && <small className="unlogged-control">unlogged · saved after the log recovered</small>}<div className="worker-models"><WorkerModel model={worker.model} defaultEffort={!!worker.pairedInstruction&&!worker.activeTurn} />{worker.pairedInstruction&&<PairedInstructionEffort instruction={worker.pairedInstruction}/>}</div><div className="worker-actions"><button aria-label={`Start ${label}`} disabled={closed||!!fresh||retiredTracks.has(worker.trackId)||realignHoldsWork(realignments,pendingRealignments,worker.trackId) || pendingInheritance.some(pending => pending.trackId === worker.trackId) || !['ready', 'held', 'sleeping'].includes(worker.phase)} onClick={() => start(worker.trackId)}>Start</button><button aria-label={`Stop ${label}`} disabled={closed||['done','fresh-releasing','failed', 'stopped', 'after-connection-loss', 'checkpoint', 'restoring'].includes(worker.phase)} onClick={() => stop(worker.trackId)}>Stop</button>{!closed&&<button aria-label={`${fresh?'Review fresh conversation for':'Start fresh for'} ${label}`} disabled={!connected||retiredTracks.has(worker.trackId)||(!fresh&&!canPrepareFresh(worker))} onClick={()=>openFresh(worker)}>{fresh?'Review fresh…':'Start fresh…'}</button>}</div></div>
      {lineage&&<p className="fresh-lineage">Continues {tracks.find(track=>track.id===lineage.source)?.label??'the original'} · {branchContext(lineage.origin,tracks.find(track=>track.id===lineage.source)?.label??'the source')} · summarized</p>}
      {successor&&<p className="fresh-lineage">Continued as {tracks.find(track=>track.id===successor.successor)?.label??'a new conversation'}<button onClick={()=>openTrack(successor.successor)}>View continuation</button></p>}
      {fresh&&!closed&&<p className="fresh-lineage" role="status">{freshPhaseText[fresh.phase]}</p>}
      {secretInputs.find(input => input.owner.kind === 'work' && input.trackId === worker.trackId) && (() => { const input = secretInputs.find(input => input.owner.kind === 'work' && input.trackId === worker.trackId)!; return <p role="status">{input.state === 'ready' ? `${input.name} was supplied to this host.` : input.state === 'prepared' || input.state === 'stopping' || input.state === 'settled' || input.state === 'restarting' ? `Preparing ${input.name} for this worker…` : input.detail}</p>; })()}
      {worker.phase === 'held' && worker.detail && <p role="status">{worker.detail}</p>}
      <ReferenceScreening track={worker.trackId} label={label} />
      <ProgressOutput label={label} preview={flows.find(flow => !flow.repairRunId && flow.trackId === worker.trackId)?.preview} session={worker.sessionId} />
      {worker.output && <div role="log" aria-label={`${label} result`}>{worker.checkpoint && !worker.checkpointPending && <p className="checkpoint-label">Checkpoint: {worker.checkpoint.sha}{worker.checkpoint.liveProcesses ? ' · processes still running' : ''}</p>}<MarkdownMessage className="worker-output" text={worker.output} /></div>}
      <Effects effects={effects.filter(effect => effect.trackId === worker.trackId && effect.turnId === (worker.activeTurn ?? worker.checkpoint?.turnId))} />
    </section>; })}
  </div>;
}
