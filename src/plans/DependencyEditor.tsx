import { useEffect, useRef, useState } from 'react';
import { PrivacyText } from '../desk/PrivacyText';
import type { Plan, PlanItem, PlanRewriteAction } from './types';

export function DependencyEditor({ initial, itemId, update, close }: {
  initial: Plan; itemId: string; update: (plan: Plan) => void; close: (plan: Plan, applied: boolean) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [plan, setPlan] = useState(initial);
  const [after, setAfter] = useState<PlanItem['after']>(initial.items.find(item => item.planItemId === itemId)?.after ?? []);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const token = useRef<Promise<string> | null>(null);
  const pending = useRef<PlanRewriteAction | null>(null);
  const active = useRef(false);
  const item = plan.items.find(item => item.planItemId === itemId)!;
  async function perform(kind: 'begin' | 'dependencies' | 'cancel') {
    if (active.current) return;
    active.current = true; setBusy(true); setError('');
    try {
      token.current ??= window.intercat.createOperationId();
      const key = await token.current;
      if (kind === 'cancel') {
        pending.current = null;
        const current = (await window.intercat.getStatus()).plans?.find(value => value.id === plan.id);
        if (current && current.rewrite?.token !== key) { update(current); close(current, false); return; }
      }
      pending.current ??= kind === 'begin' ? { kind, planId: initial.id, itemId, token: key, revision: initial.rev }
        : kind === 'cancel' ? { kind, planId: plan.id, token: key }
        : { kind, planId: plan.id, token: key, revision: plan.rev, opId: await window.intercat.createOperationId(), after };
      const result = await window.intercat.editPlan(pending.current);
      if (!result) throw new Error('The plan is unavailable.');
      const completed = pending.current.kind;
      pending.current = null; update(result); setPlan(result);
      if (completed === 'begin') setReady(true);
      else close(result, completed === 'dependencies');
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'The dependency change could not be saved.'); }
    finally { active.current = false; setBusy(false); }
  }
  useEffect(() => {
    dialog.current?.showModal(); void perform('begin');
    const previous = document.activeElement;
    return () => { if (previous instanceof HTMLElement && previous.isConnected) previous.focus(); };
  }, []);
  useEffect(() => {
    if (!ready) return;
    const timer = setInterval(() => {
      void token.current?.then(key => window.intercat.editPlan({ kind: 'renew', planId: plan.id, token: key }))
        .catch(() => setError('Editing was interrupted. Close this editor and open the item again.'));
    }, 10_000);
    return () => clearInterval(timer);
  }, [ready, plan.id]);
  return <dialog ref={dialog} className="plan-review plan-dependency-editor" aria-labelledby="plan-dependency-title"
    onCancel={event => { event.preventDefault(); if (!busy) void perform('cancel'); }}>
    <header><h2 id="plan-dependency-title">Change dependencies</h2></header>
    <div className="plan-review-body">
      <p><PrivacyText text={item.title} /></p>
      <p role="status">{ready ? 'This item waits while you change its dependencies.' : 'Holding this item…'}</p>
      <fieldset disabled={!ready || busy || !!pending.current}>
        <legend>Wait for these items</legend>
        <label><input type="checkbox" checked={!after.length} onChange={() => setAfter([])} />None</label>
        {plan.items.filter(parent => parent.planItemId !== itemId).map(parent => {
          const selected = after.find(value => value.id === parent.planItemId);
          return <div key={parent.planItemId}>
            <label><input type="checkbox" checked={!!selected} onChange={() => setAfter(current => selected ? current.filter(value => value.id !== parent.planItemId) : [...current, { id: parent.planItemId, inherit: parent.folderId === item.folderId }])} /><PrivacyText text={parent.title} /></label>
            {selected && parent.folderId === item.folderId && <label className="plan-inherit-choice"><input type="checkbox" checked={selected.inherit} onChange={() => setAfter(current => current.map(value => value.id === parent.planItemId ? { ...value, inherit: !value.inherit } : value))} />Inherit files</label>}
          </div>;
        })}
      </fieldset>
      {error && <div role="alert"><p>{error}</p>{pending.current && <button disabled={busy} onClick={() => { void perform(pending.current?.kind === 'begin' ? 'begin' : pending.current?.kind === 'cancel' ? 'cancel' : 'dependencies'); }}>Retry dependency action</button>}</div>}
    </div>
    <footer><button disabled={busy} onClick={() => { void perform('cancel'); }}>Keep current dependencies</button>
      <button className="start-plan" disabled={!ready || busy || !!pending.current} onClick={() => { void perform('dependencies'); }}>Apply dependencies</button></footer>
  </dialog>;
}
