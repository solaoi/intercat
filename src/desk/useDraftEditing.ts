import { useEffect, useRef, useState } from 'react';
import type { Utterance } from '../bridge';
import { useDrafts, type EditLease } from './drafts';

type Drafts = ReturnType<typeof useDrafts>;
export function useDraftEditing(drafts: Drafts, update: (word: Utterance) => void) {
  const [errors, setErrors] = useState<Record<string, string>>({});
  const preparing = useRef(new Map<string, Promise<EditLease | null>>());
  const updateRef = useRef(update);
  updateRef.current = update;
  function error(id: string, message = '') { setErrors(values => ({ ...values, [id]: message })); }
  function observe(word: Utterance) {
    updateRef.current(word);
    for (const [id, entry] of Object.entries(drafts.book.entries)) {
      if (entry.context.kind !== 'edit' || entry.context.inputId !== word.id || entry.pending) continue;
      if (word.state === 'cut') { drafts.remove(id); continue; }
      if (word.editHold) drafts.updateEdit(id, { ...entry.context, lease: word.editHold, state: 'editing' });
    }
  }
  async function acquire(id: string): Promise<EditLease | null> {
    const entry = drafts.book.entries[id];
    if (!entry || entry.context.kind !== 'edit') return null;
    const context = entry.context;
    if (context.state === 'sent') return null;
    if (context.lease) {
      try {
        const word = await window.intercat.editUtterance({ kind: 'renew', token: context.lease.token });
        if (word.editHold && word.revision === word.editHold.revision) {
          observe(word); error(id); return word.editHold;
        }
        // A Stop can change the word's revision while leaving this lease open.
        await window.intercat.editUtterance({ kind: 'end', token: context.lease.token, revision: context.lease.revision, edit: { kind: 'cancel' } });
      } catch { /* Re-read ownership before trying to acquire a new lease. */ }
    }
    const status = await window.intercat.getStatus();
    const word = status.utterances?.find(word => word.id === context.inputId);
    if (!word) throw new Error('The original request is unavailable.');
    if (word.interruptStarted || ['starting', 'sent', 'landed'].includes(word.state)) {
      drafts.updateEdit(id, { ...context, lease: null, state: 'sent' });
      error(id); return null;
    }
    if (word.editHold) throw new Error('This request is being edited in another window.');
    const saved = await window.intercat.editUtterance({ kind: 'begin', token: await window.intercat.createOperationId(), utteranceId: word.id, revision: word.revision });
    if (!saved.editHold) throw new Error('The request could not be held for editing.');
    drafts.updateEdit(id, { ...context, lease: saved.editHold, state: 'editing' });
    observe(saved); error(id); return saved.editHold;
  }
  function ensure(id: string): Promise<EditLease | null> {
    const pending = preparing.current.get(id);
    if (pending) return pending;
    const task = acquire(id).catch(cause => { error(id, cause instanceof Error ? cause.message : 'The editing lease changed.'); throw cause; });
    preparing.current.set(id, task);
    void task.finally(() => preparing.current.delete(id)).catch(() => {});
    return task;
  }
  useEffect(() => {
    const id = drafts.book.active;
    const entry = drafts.book.entries[id];
    if (drafts.ready && entry?.context.kind === 'edit' && !entry.pending) void ensure(id).catch(() => {});
  }, [drafts, drafts.ready, drafts.book.active]);
  useEffect(() => {
    const timer = setInterval(() => {
      for (const [id, entry] of Object.entries(drafts.book.entries)) {
        if (entry.context.kind !== 'edit' || !entry.context.lease || entry.context.state === 'sent') continue;
        void window.intercat.editUtterance({ kind: 'renew', token: entry.context.lease.token }).catch(() => {
          if (!entry.pending) error(id, 'Editing was interrupted. Update the request to recheck its ownership.');
        });
      }
    }, 10_000);
    return () => clearInterval(timer);
  }, [drafts]);
  async function submit(): Promise<boolean> {
    const id = drafts.book.active;
    const entry = drafts.entry;
    if (entry.context.kind !== 'edit') return false;
    let pending = entry.pending;
    if (!pending) {
      const lease = await ensure(id);
      if (!lease) return false;
      pending = await drafts.prepare({ kind: 'edit', request: { kind: 'end', token: lease.token, revision: lease.revision, edit: { kind: 'text', text: drafts.entry.text, sources: (drafts.entry.sources ?? []).map(source => ({ attachmentId: source.id, role: source.role, after: source.after })) } } });
    } else await drafts.flush();
    if (pending.kind !== 'edit') throw new Error('A different draft operation is pending.');
    const saved = await window.intercat.editUtterance(pending.request);
    updateRef.current(saved); drafts.finish(); error(id); return true;
  }
  async function cancel() {
    const id = drafts.book.active;
    const entry = drafts.entry;
    if (entry.context.kind !== 'edit') return;
    if (entry.pending) { drafts.back(); return; }
    await preparing.current.get(id)?.catch(() => {});
    const current = drafts.book.entries[id]?.context;
    if (current?.kind === 'edit' && current.lease) {
      try { observe(await window.intercat.editUtterance({ kind: 'end', token: current.lease.token, revision: current.lease.revision, edit: { kind: 'cancel' } })); }
      catch { /* A replaced or expired lease remains held by its current owner. */ }
    }
    drafts.remove(id); error(id);
  }
  return { submit, cancel, ensure, observe, error: errors[drafts.book.active] ?? '' };
}
