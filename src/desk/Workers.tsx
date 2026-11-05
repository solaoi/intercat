import type { UnloggedControl, Approval, LiveTrack, WorkerState, Prompt, PromptResponse, SecretEntry, SecretInputState } from '../bridge';
import { QuestionCard } from './QuestionCard';
import { ApprovalCard } from './ApprovalCard';
import { Effects } from './Effects';
import type { Effect, OutputFlow } from '../bridge';
import { ProgressOutput } from './ProgressOutput';
import { MarkdownMessage } from '../markdown/MarkdownMessage';


export function Workers({ unloggedControls = [], workers, tracks, flows = [], effects = [], secretInputs = [], provideSecret, start, stop, answer, respond }: { unloggedControls?: UnloggedControl[]; secretInputs?: SecretInputState[]; provideSecret: (worker: WorkerState, prompt: Prompt, entry: SecretEntry) => Promise<void>; workers: WorkerState[]; tracks: LiveTrack[]; flows?: OutputFlow[]; effects?: Effect[]; start: (id: string) => void; stop: (id: string) => void; answer: (worker: WorkerState, card: Approval, allow: boolean) => Promise<void>; respond: (worker: WorkerState, prompt: Prompt, response: PromptResponse) => Promise<void> }) {
  return <div className="workers">
    {workers.flatMap(worker => worker.prompts.map(prompt => <QuestionCard provideSecret={(prompt, entry) => provideSecret(worker, prompt, entry)} key={`${worker.trackId}:${prompt.request.userInputId}:${prompt.revision}`} stopping={['stopping', 'reconnecting', 'lost', 'needs-recovery'].includes(worker.phase)} prompt={prompt} label={`Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`} respond={(prompt, response) => respond(worker, prompt, response)} />))}
    {workers.flatMap(worker => worker.approvals.map(card => <ApprovalCard key={`${card.id}:${JSON.stringify(card.requirement)}`} stopping={['stopping', 'reconnecting', 'lost', 'needs-recovery'].includes(worker.phase)} detail={worker.detail} label={`Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`} card={card} answer={(card, allow) => answer(worker, card, allow)} />))}
    {workers.map(worker => { const label = `Worker ${tracks.find(track => track.id === worker.trackId)?.label ?? ''}`; return <section key={worker.trackId}>
      <div className="worker-controls"><strong>{label}</strong><span>{['stopped', 'after-connection-loss'].includes(worker.phase) && worker.detail ? worker.detail : worker.phase}</span>{!['working','starting'].includes(worker.phase) && unloggedControls.some(control => control.paused !== false && (!control.trackId || control.trackId === worker.trackId)) && <small className="unlogged-control">unlogged · saved after the log recovered</small>}<div className="worker-actions"><button aria-label={`Start ${label}`} disabled={!['ready', 'held'].includes(worker.phase)} onClick={() => start(worker.trackId)}>Start</button><button aria-label={`Stop ${label}`} disabled={['failed', 'stopped', 'after-connection-loss', 'checkpoint', 'restoring'].includes(worker.phase)} onClick={() => stop(worker.trackId)}>Stop</button></div></div>
      {secretInputs.find(input => input.owner.kind === 'work' && input.trackId === worker.trackId) && (() => { const input = secretInputs.find(input => input.owner.kind === 'work' && input.trackId === worker.trackId)!; return <p role="status">{input.state === 'ready' ? `${input.name} was supplied to this host.` : input.state === 'prepared' || input.state === 'stopping' || input.state === 'settled' || input.state === 'restarting' ? `Preparing ${input.name} for this worker…` : input.detail}</p>; })()}
      <ProgressOutput label={label} preview={flows.find(flow => !flow.repairRunId && flow.trackId === worker.trackId)?.preview} session={worker.sessionId} />
      {worker.output && <div role="log" aria-label={`${label} result`}>{worker.checkpoint && !worker.checkpointPending && <p className="checkpoint-label">Checkpoint: {worker.checkpoint.sha}{worker.checkpoint.liveProcesses ? ' · processes still running' : ''}</p>}<MarkdownMessage className="worker-output" text={worker.output} /></div>}
      <Effects effects={effects.filter(effect => effect.trackId === worker.trackId && effect.turnId === (worker.activeTurn ?? worker.checkpoint?.turnId))} />
    </section>; })}
  </div>;
}
