import { useRef, useState } from 'react';
import { PrivacyText } from '../desk/PrivacyText';
import type { Plan, PlanRewriteAction } from './types';

export function ItemRevision({ plan, update, edit }: { plan: Plan; update: (plan: Plan) => void; edit: (plan: Plan, itemId: string) => void }) {
  const revision = plan.rewrite;
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<PlanRewriteAction | null>(null);
  if (!revision) return null;
  async function perform(kind: 'apply' | 'retry' | 'cancel') {
    if (!revision || busy) return;
    setBusy(true); setError('');
    try {
      pending.current ??= kind === 'cancel' ? { kind, planId: plan.id, token: revision.token }
        : { kind, planId: plan.id, token: revision.token, revision: plan.rev, opId: await window.intercat.createOperationId() };
      const result = await window.intercat.editPlan(pending.current);
      if (!result) throw new Error('The item revision is unavailable.');
      pending.current = null; update(result);
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'The revision could not be saved.'); }
    finally { setBusy(false); }
  }
  return <section className="plan-item-revision" aria-label="Item revision">
    <h3><PrivacyText text={revision.before.title} /></h3>
    {revision.words === null ? <>
      <p>This item waits while its instructions are being edited.</p>
      <button disabled={busy} onClick={() => edit(plan, revision.itemId)}>Continue editing</button>
    </> : <>
      <p>Your revision: <PrivacyText text={revision.words} /></p>
      {revision.proposal ? <>
        <h4>Revised item</h4><p><PrivacyText text={revision.proposal.title} /></p>
        <p className="plan-instruction"><PrivacyText text={revision.proposal.instruction} /></p>
        {!!revision.proposal.constraints.length && <ul>{revision.proposal.constraints.map((line, index) => <li key={index}><PrivacyText text={line} /></li>)}</ul>}
        <p>Done when: <PrivacyText text={revision.proposal.doneWhen} /></p>
        <button className="start-plan" disabled={busy || !!pending.current} onClick={() => { void perform('apply'); }}>Use revised item</button>
      </> : <>
        <p role="status">{['queued', 'running'].includes(plan.generation.state) ? 'Revising item…' : 'The item revision needs attention.'}</p>
        {plan.generation.detail && <p><PrivacyText text={plan.generation.detail} /></p>}
        {!['queued', 'running'].includes(plan.generation.state) && <button disabled={busy || !!pending.current} onClick={() => { void perform('retry'); }}>Retry revision</button>}
      </>}
    </>}
    <button disabled={busy || !!pending.current} onClick={() => { void perform('cancel'); }}>Keep current item</button>
    {error && <div role="alert"><p>{error}</p><button disabled={busy} onClick={() => { void perform(pending.current?.kind === 'apply' ? 'apply' : pending.current?.kind === 'retry' ? 'retry' : 'cancel'); }}>Retry revision action</button></div>}
  </section>;
}
