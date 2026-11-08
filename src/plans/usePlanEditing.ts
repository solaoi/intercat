import { useEffect, useRef, useState } from 'react';
import type { Plan } from './types';
import type { useDrafts } from '../desk/drafts';

export function usePlanEditing(drafts: ReturnType<typeof useDrafts>, update: (plan: Plan) => void, review: (plan: Plan, itemId: string) => void) {
  const [error, setError] = useState('');
  const opening = useRef(new Map<string, string>());
  async function open(plan: Plan, itemId: string) {
    const item = plan.items.find(item => item.planItemId === itemId);
    if (!item) throw new Error('The plan item is unavailable.');
    const saved = Object.values(drafts.book.entries).find(entry => entry.context.kind === 'plan-item' && entry.context.planId === plan.id && entry.context.itemId === itemId);
    const existing = saved?.context.kind === 'plan-item' ? saved.context : null;
    const key = `${plan.id}/${itemId}`;
    const token = opening.current.get(key) ?? await window.intercat.createOperationId();
    opening.current.set(key, token);
    const result = plan.rewrite && (existing?.token === plan.rewrite.token || plan.rewrite.itemId === itemId)
      ? await window.intercat.editPlan({ kind: 'renew', planId: plan.id, token: plan.rewrite.token })
      : await window.intercat.editPlan({ kind: 'begin', planId: plan.id, itemId, token, revision: plan.rev });
    if (!result?.rewrite || result.rewrite.words !== null) throw new Error('Review the submitted revision before editing again.');
    update(result);
    opening.current.delete(key);
    drafts.open({ kind: 'plan-item', planId: plan.id, itemId, title: item.title, token: result.rewrite.token, revision: result.rev });
    setError('');
    requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
  }
  useEffect(() => {
    const timer = setInterval(() => {
      for (const entry of Object.values(drafts.book.entries)) {
        if (entry.context.kind !== 'plan-item' || entry.pending) continue;
        const context = entry.context;
        void window.intercat.editPlan({ kind: 'renew', planId: context.planId, token: context.token }).catch(() => setError('Editing was interrupted. Reopen this item before submitting.'));
      }
    }, 10_000);
    return () => clearInterval(timer);
  }, [drafts]);
  async function submit() {
    const entry = drafts.entry;
    if (entry.context.kind !== 'plan-item') return;
    const context = entry.context;
    try {
      const pending = await drafts.prepare({ kind: 'plan-item', request: { kind: 'submit', planId: context.planId, token: context.token, revision: context.revision, opId: await window.intercat.createOperationId(), text: entry.text } });
      if (pending.kind !== 'plan-item') throw new Error('Another draft operation is pending.');
      const result = await window.intercat.editPlan(pending.request);
      if (!result) throw new Error('The item revision was not saved.');
      update(result); drafts.finish(); setError(''); review(result, context.itemId);
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'The revision could not be saved.'); throw cause; }
  }
  async function cancel() {
    const entry = drafts.entry;
    if (entry.context.kind !== 'plan-item') return;
    try {
      const result = await window.intercat.editPlan({ kind: 'cancel', planId: entry.context.planId, token: entry.context.token });
      if (result) update(result);
      drafts.remove(drafts.book.active); setError('');
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'The edit could not be closed.'); }
  }
  async function reopen() {
    const context = drafts.entry.context;
    if (context.kind !== 'plan-item') return;
    try {
      const plan = await window.intercat.editPlan({ kind: 'cancel', planId: context.planId, token: context.token });
      if (!plan) throw new Error('The plan is unavailable.');
      drafts.clearPending();
      await open(plan, context.itemId);
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'This item could not be reopened.'); }
  }
  return { open, submit, cancel, reopen, error };
}
