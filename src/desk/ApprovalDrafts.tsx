import { createContext, useContext, useRef, useState } from 'react';
import type { Approval, ApprovalResponse, DaemonStatus, SecretOwner } from '../bridge';
import { approvalDraftId, type ApprovalDraftContext, type ApprovalSubmission, type useDrafts } from './drafts';

export const ApprovalDraftStore = createContext<ReturnType<typeof useDrafts> | null>(null);
type Store = ReturnType<typeof useDrafts>;

export function approvalContext(owner: SecretOwner, trackId: string, sessionId: string | null, label: string, card: Approval): ApprovalDraftContext | null {
  return sessionId && card.replyMode ? { kind: 'approval', owner, trackId, sessionId, label, approvalId: card.id, requirement: card.requirement, mode: card.replyMode } : null;
}

function useStore() {
  const store = useContext(ApprovalDraftStore);
  if (!store) throw new Error('Approval replies require the session draft store.');
  return store;
}

async function send(request: ApprovalSubmission) {
  const answer = { opId: request.opId, approvalId: request.approvalId, requirement: request.requirement, response: request.response };
  if (request.owner.kind === 'work') await window.intercat.answerWorker({ ...answer, trackId: request.trackId });
  else await window.intercat.answerRepair({ ...answer, id: request.owner.runId });
}

async function prepareReply(store: Store, context: ApprovalDraftContext, response: Extract<ApprovalResponse, { action: 'reply' }>) {
  if (!store.ready) throw new Error('Wait for saved replies to load.');
  const id = approvalDraftId(context);
  let pending = store.book.entries[id]?.pending;
  if (!pending) {
    const request: ApprovalSubmission = { owner: context.owner, trackId: context.trackId, sessionId: context.sessionId, approvalId: context.approvalId, requirement: context.requirement, opId: await window.intercat.createOperationId(), response };
    store.change(book => {
      book.entries[id] ??= { context, text: response.text, target: { mode: 'lock', trackId: context.trackId }, attachments: [], pending: null };
      book.entries[id].pending ??= { kind: 'approval', request };
    });
    pending = store.book.entries[id].pending;
  }
  if (pending?.kind !== 'approval') throw new Error('A different draft operation is pending.');
  // Stopping must remain immediate even when the database cannot save drafts.
  if (pending.request.response.mode === 'feedback') await store.flush();
  return pending.request;
}

export function useApprovalDraft(context: ApprovalDraftContext | null) {
  const store = useStore();
  const id = context ? approvalDraftId(context) : '';
  const stored = store.book.entries[id];
  const entry = stored?.context.kind === 'approval' ? stored : undefined;
  function setText(text: string) {
    if (!context || entry?.pending) return;
    store.change(book => {
      book.entries[id] ??= { context, text: '', target: { mode: 'lock', trackId: context.trackId }, attachments: [], pending: null };
      book.entries[id].text = text;
    });
  }
  async function prepare(response: Extract<ApprovalResponse, { action: 'reply' }>) {
    if (!context) throw new Error('The approval reply is no longer available.');
    return prepareReply(store, context, response);
  }
  return { ready: store.ready, text: entry?.text ?? '', pending: entry?.pending, setText, prepare, finish: () => store.remove(id) };
}

export function ApprovalReplyRecovery({ status, repairVisible }: { status: DaemonStatus; repairVisible: boolean }) {
  const store = useStore();
  const [busy, setBusy] = useState<string | null>(null);
  const sending = useRef(false);
  const [error, setError] = useState('');
  const replies = Object.entries(store.book.entries).filter(([, entry]) => {
    const context = entry.context;
    if (context.kind !== 'approval' || (!entry.text && !entry.pending)) return false;
    const same = (card: { id: string; requirement: unknown; replyMode?: string | null }) => card.id === context.approvalId && JSON.stringify(card.requirement) === JSON.stringify(context.requirement) && card.replyMode === context.mode;
    if (context.owner.kind === 'work') return !status.workers?.some(worker => ['approval', 'working'].includes(worker.phase) && worker.trackId === context.trackId && worker.sessionId === context.sessionId && worker.approvals.some(same));
    const owner = context.owner;
    return !repairVisible || !status.repairRuns?.some(run => run.phase === 'running' && run.id === owner.runId && run.session === context.sessionId && run.approvals.some(same));
  });
  async function retry(id: string) {
    if (sending.current) return;
    const entry = store.book.entries[id];
    if (entry.context.kind !== 'approval') return;
    sending.current = true;
    setBusy(id); setError('');
    try {
      const request = await prepareReply(store, entry.context, { action: 'reply', text: entry.text, mode: entry.context.mode });
      await send(request); store.remove(id);
    }
    catch (failure) { setError(failure instanceof Error ? failure.message : 'The saved reply could not be confirmed.'); }
    finally { sending.current = false; setBusy(null); }
  }
  function keep(id: string) {
    const recoveredId = `recovered:${crypto.randomUUID()}`;
    store.change(book => {
      const entry = book.entries[id];
      if (entry.context.kind !== 'approval') return;
      const context = entry.context;
      book.entries[recoveredId] = { ...entry, context: context.owner.kind === 'repair' ? { kind: 'repair', id: context.owner.runId } : { kind: 'work' }, pending: null, target: { mode: 'lock', trackId: context.trackId } };
      if (!entry.pending) delete book.entries[id];
      book.stack = [...book.stack.filter(previous => previous !== book.active), book.active];
      book.active = recoveredId;
    });
    requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
  }
  if (!replies.length) return null;
  return <section className="approval-recovery" aria-label="Saved approval replies">
    {replies.map(([id, entry]) => <details key={id} open>
      <summary>Saved approval reply · {(entry.context as ApprovalDraftContext).label}</summary>
      <pre>{entry.text}</pre>
      <div className="desk-actions">
        <button disabled={busy !== null} onClick={() => { void retry(id); }}>{entry.pending ? 'Retry response' : (entry.context as ApprovalDraftContext).mode === 'feedback' ? 'Decline and reply' : 'Stop and send'}</button>
        <button disabled={busy !== null} onClick={() => keep(id)}>{entry.pending ? 'Copy to draft' : 'Keep as draft'}</button>
        <button disabled={busy !== null} onClick={() => store.remove(id)}>Discard saved text</button>
      </div>
    </details>)}
    {error && <p role="alert">{error}</p>}
  </section>;
}
