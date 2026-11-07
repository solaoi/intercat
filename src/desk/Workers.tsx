import { useState } from 'react';
import type { UnloggedControl, Approval, ApprovalResponse, LiveTrack, WorkerState, Prompt, PromptResponse, SecretEntry, SecretInputState } from '../bridge';
import { QuestionCard } from './QuestionCard';
import { ApprovalCard } from './ApprovalCard';
import { approvalContext } from './ApprovalDrafts';
import { Effects } from './Effects';
import type { Effect, OutputFlow } from '../bridge';
import { ProgressOutput } from './ProgressOutput';
import { MarkdownMessage } from '../markdown/MarkdownMessage';
import { ReferenceScreening } from './ReferenceScreening';
import type { AttentionCard } from './NeedsYou';
import { WorkerModel } from './WorkerModel';


type Props = { unloggedControls?: UnloggedControl[]; secretInputs?: SecretInputState[]; workers: WorkerState[]; tracks: LiveTrack[]; flows?: OutputFlow[]; effects?: Effect[]; start: (id: string) => void; stop: (id: string) => void; canDropFiles: boolean; dropFiles: (files: File[], trackId: string) => void };
type CardProps = Pick<Props, 'workers' | 'tracks'> & {
  provideSecret: (worker: WorkerState, prompt: Prompt, entry: SecretEntry) => Promise<void>;
  answer: (opId: string, worker: WorkerState, card: Approval, response: ApprovalResponse) => Promise<void>;
  respond: (worker: WorkerState, prompt: Prompt, response: PromptResponse) => Promise<void>;
};

export function workerCards({ workers, tracks, provideSecret, answer, respond }: CardProps): AttentionCard[] {
  return [
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

export function Workers({ unloggedControls = [], workers, tracks, flows = [], effects = [], secretInputs = [], start, stop, canDropFiles, dropFiles }: Props) {
  const [dropTrack, setDropTrack] = useState<string | null>(null);
  return <div className="workers">
    {workers.map(worker => { const track = tracks.find(track => track.id === worker.trackId); const label = `Worker ${track?.label ?? ''}`; const acceptsFiles = canDropFiles && !!track; return <section key={worker.trackId} data-worker-card={worker.trackId} className={dropTrack === worker.trackId && acceptsFiles ? 'file-drop-active' : undefined}
      onDragOver={event => {
        if (!event.dataTransfer.types.includes('Files')) return;
        event.preventDefault(); event.stopPropagation(); event.dataTransfer.dropEffect = acceptsFiles ? 'copy' : 'none';
        setDropTrack(acceptsFiles ? worker.trackId : null);
      }}
      onDragLeave={event => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) setDropTrack(null); }}
      onDrop={event => { if (!event.dataTransfer.types.includes('Files')) return; event.preventDefault(); event.stopPropagation(); setDropTrack(null); if (acceptsFiles) dropFiles(Array.from(event.dataTransfer.files), worker.trackId); }}>
      {dropTrack === worker.trackId && acceptsFiles && <div className="card-file-drop"><span>Drop to attach · next words to {label}</span></div>}
      <div className="worker-controls"><strong>{label}</strong><span>{['stopped', 'after-connection-loss'].includes(worker.phase) && worker.detail ? worker.detail : worker.phase}</span>{!['working','starting'].includes(worker.phase) && unloggedControls.some(control => control.paused !== false && (!control.trackId || control.trackId === worker.trackId)) && <small className="unlogged-control">unlogged · saved after the log recovered</small>}<WorkerModel model={worker.model} /><div className="worker-actions"><button aria-label={`Start ${label}`} disabled={!['ready', 'held'].includes(worker.phase)} onClick={() => start(worker.trackId)}>Start</button><button aria-label={`Stop ${label}`} disabled={['failed', 'stopped', 'after-connection-loss', 'checkpoint', 'restoring'].includes(worker.phase)} onClick={() => stop(worker.trackId)}>Stop</button></div></div>
      {secretInputs.find(input => input.owner.kind === 'work' && input.trackId === worker.trackId) && (() => { const input = secretInputs.find(input => input.owner.kind === 'work' && input.trackId === worker.trackId)!; return <p role="status">{input.state === 'ready' ? `${input.name} was supplied to this host.` : input.state === 'prepared' || input.state === 'stopping' || input.state === 'settled' || input.state === 'restarting' ? `Preparing ${input.name} for this worker…` : input.detail}</p>; })()}
      {worker.phase === 'held' && worker.detail && <p role="status">{worker.detail}</p>}
      <ReferenceScreening track={worker.trackId} label={label} />
      <ProgressOutput label={label} preview={flows.find(flow => !flow.repairRunId && flow.trackId === worker.trackId)?.preview} session={worker.sessionId} />
      {worker.output && <div role="log" aria-label={`${label} result`}>{worker.checkpoint && !worker.checkpointPending && <p className="checkpoint-label">Checkpoint: {worker.checkpoint.sha}{worker.checkpoint.liveProcesses ? ' · processes still running' : ''}</p>}<MarkdownMessage className="worker-output" text={worker.output} /></div>}
      <Effects effects={effects.filter(effect => effect.trackId === worker.trackId && effect.turnId === (worker.activeTurn ?? worker.checkpoint?.turnId))} />
    </section>; })}
  </div>;
}
