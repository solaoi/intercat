import type { Plan, PlanAction } from './types';
import { waitingForMemory } from '../scheduling';
import type { HostMemoryStatus, LiveTrack } from '../bridge';

export function planLabel(plan: Plan, hostMemory?: HostMemoryStatus): string {
  if (waitingForMemory({ hostMemory }, plan.generation.id)) return 'queued · waiting for memory';
  if (plan.state === 'single-job') return 'sent as one job';
  if (plan.rewrite) return plan.rewrite.words === null ? 'editing an item' : plan.rewrite.proposal ? 'review item revision' : 'revising an item';
  if (plan.generation.state === 'queued') return 'planning · queued';
  if (plan.generation.state === 'running') return 'planning';
  if (plan.generation.state === 'timed-out') return 'planning · timed out';
  if (['invalid', 'unlock-required'].includes(plan.generation.state)) return 'planning · invalid plan';
  if (plan.generation.state === 'failed') return 'planning · stopped';
  if (plan.state === 'held') return plan.items.every(item => ['planned', 'editing', 'cut'].includes(item.state)) ? 'held · nothing has run' : 'held';
  if (plan.state === 'running') return 'running';
  if (plan.state === 'done') return 'done';
  return plan.executeAllowed ? 'planned · nothing has run' : 'waiting for your go';
}

export function PlanControls({ plan, tracks, busy, pending, connected, paused, stale = false, primary = false, review, start, command }: {
  plan: Plan; busy: boolean; pending: boolean; connected: boolean; paused: boolean; stale?: boolean; primary?: boolean;
  tracks: Pick<LiveTrack, 'id' | 'label'>[];
  review: () => void; start: () => void; command: (action: PlanAction) => void;
}) {
  const ready = !plan.rewrite && plan.generation.state === 'ready' && ['draft', 'held'].includes(plan.state) && plan.items.some(item => item.state === 'planned');
  const remaining = plan.items.some(item => ['planned', 'editing'].includes(item.state));
  const destinations = new Set(plan.items.filter(item => item.state !== 'cut').map(item => item.trackRef)).size;
  const disabled = busy || pending || !connected || stale;
  return <div className="plan-actions">
    <button disabled={busy || (!plan.items.length && !plan.generation.detail)} onClick={review}>{stale ? 'Review current plan' : 'Review plan'}</button>
    {ready && <button className={primary ? 'start-plan' : undefined} disabled={disabled || paused} onClick={start}>Start plan · {destinations} {destinations === 1 ? 'track' : 'tracks'}</button>}
    {ready && plan.state === 'draft' && plan.startMode === 'after-delay' && plan.executeAllowed && <button disabled={disabled} onClick={() => command({ kind: 'hold' })}>Hold</button>}
    {plan.state === 'running' && remaining && <button disabled={disabled} onClick={() => command({ kind: 'hold' })}>Hold the rest</button>}
    <PlanFailureActions plan={plan} tracks={tracks} disabled={disabled} paused={paused} command={command} />
  </div>;
}

export function PlanFailureActions({ plan, tracks, disabled, paused, command }: {
  plan: Plan; tracks: Pick<LiveTrack, 'id' | 'label'>[]; disabled: boolean; paused: boolean; command: (action: PlanAction) => void;
}) {
  if (plan.rewrite) return null;
  if (plan.generation.state === 'unlock-required') return <div role="group" aria-label="Unlock and split?" className="plan-lock-choice">
    <span>Unlock and split?</span>
    <button disabled={disabled} onClick={() => command({ kind: 'resolve-lock', unlock: true })}>Yes</button>
    <button disabled={disabled || !plan.executeAllowed || paused} title={!plan.executeAllowed ? 'A plan-only request cannot run as one job.' : undefined} onClick={() => command({ kind: 'resolve-lock', unlock: false })}>Keep on {tracks.find(track => track.id === plan.lockedTrack)?.label ?? 'this track'}</button>
  </div>;
  return ['failed', 'invalid', 'timed-out'].includes(plan.generation.state) ? <>
      <button disabled={disabled} onClick={() => command({ kind: 'retry' })}>Retry planning</button>
      {plan.executeAllowed && <button disabled={disabled || paused} onClick={() => command({ kind: 'run-as-one' })}>Run as one job</button>}
    </> : null;
}
