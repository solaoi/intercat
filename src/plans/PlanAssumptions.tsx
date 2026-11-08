import { useRef, useState } from 'react';
import type { AttentionCard } from '../desk/NeedsYou';
import { PrivacyText } from '../desk/PrivacyText';
import type { PlanAssumptionCard, PlanCommand } from './types';

function Assumption({ value, disabled, reply }: { value: PlanAssumptionCard; disabled: boolean; reply: (value: PlanAssumptionCard) => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<PlanCommand | null>(null);
  async function keep() {
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), planId: value.planId,
        revision: value.planRevision, action: { kind: 'keep-assumption', assumptionId: value.assumptionId } };
      await window.intercat.commandPlan(pending.current);
      pending.current = null;
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : 'The choice could not be saved.');
    } finally { setBusy(false); }
  }
  return <div className="assumption-card" aria-label="Planning assumption">
    <p>Plan assumed · <PrivacyText text={value.detail.replace(/^Assumed: /, '')} /></p>
    <details><summary>Details</summary><p><PrivacyText text={value.text} /></p><p>For <PrivacyText text={value.label} /></p></details>
    <p className="assumption-note">Work can continue. Unanswered assumptions are reviewed at commit.</p>
    {error && <p role="alert">{error}</p>}
    <div className="assumption-actions" data-card-actions>
      <button disabled={disabled || busy} onClick={() => { void keep(); }}>{pending.current ? 'Retry saving answer' : 'Keep'}</button>
      <button disabled={disabled || busy || !!pending.current} onClick={() => reply(value)}>Change</button>
    </div>
  </div>;
}

export function planAssumptionCards(values: PlanAssumptionCard[], disabled: boolean, reply: (value: PlanAssumptionCard) => void): AttentionCard[] {
  return values.map(value => ({ id: `planning-assumption-${value.id}`, kind: 'assumption', version: value.planRevision,
    title: value.detail, content: <Assumption key={`${value.id}:${value.planRevision}`} value={value} disabled={disabled} reply={reply} /> }));
}
