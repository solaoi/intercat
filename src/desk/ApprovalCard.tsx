import { useEffect, useId, useRef, useState } from 'react';
import type { Approval, ApprovalResponse } from '../bridge';
import type { ApprovalDraftContext } from './drafts';
import { useApprovalDraft } from './ApprovalDrafts';

export function ApprovalCard({ stopping, detail, label, card, draftContext, answer }: { stopping: boolean; detail: string | null; label: string; card: Approval; draftContext: ApprovalDraftContext | null; answer: (opId: string, card: Approval, response: ApprovalResponse) => Promise<void> }) {
  const subject = useRef<HTMLPreElement>(null);
  const [read, setRead] = useState(false);
  const [answering, setAnswering] = useState(false);
  const [replying, setReplying] = useState(false);
  const draft = useApprovalDraft(draftContext);
  const text = draft.text;
  const [error, setError] = useState('');
  const sending = useRef(false);
  const pending = useRef<{ signature: string; opId: string } | null>(null);
  const replyButton = useRef<HTMLButtonElement>(null);
  const helpId = useId();
  const dismissed = useRef(false);
  useEffect(() => { if (text && !dismissed.current) setReplying(true); }, [text]);
  const validReply = !!text.trim() && new TextEncoder().encode(text).byteLength <= 16 * 1024 && !text.includes('\0');
  function cancelReply() {
    dismissed.current = true;
    setReplying(false);
    requestAnimationFrame(() => replyButton.current?.focus());
  }
  async function decide(response: ApprovalResponse) {
    if (sending.current) return;
    sending.current = true;
    setAnswering(true); setError('');
    try {
      if (response.action === 'reply') {
        const request = await draft.prepare(response);
        await answer(request.opId, card, request.response);
        draft.finish();
      } else {
        const signature = JSON.stringify([card.id, card.requirement, response]);
        if (pending.current?.signature !== signature) pending.current = { signature, opId: await window.intercat.createOperationId() };
        await answer(pending.current.opId, card, response);
      }
      setReplying(false);
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The response could not be confirmed. Retry sends the same response.'); }
    finally { sending.current = false; setAnswering(false); }
  }
  useEffect(() => { const element = subject.current; setRead(!!element && element.scrollHeight <= element.clientHeight + 1); }, [card.id, card.subject, JSON.stringify(card.requirement)]);
  return <div className="approval-card" role="group" aria-label={`Approval for ${label}`}>
    <strong>{label} · {card.reason.message}</strong>
    <pre ref={subject} tabIndex={0} onScroll={event => { const element = event.currentTarget; if (element.scrollTop + element.clientHeight >= element.scrollHeight - 1) setRead(true); }}>{card.subject}</pre>
    {card.deletion && <p>Snapshot {card.deletion.sha.slice(0, 8)} saved · {card.deletion.files} files covered{card.deletion.complete ? '' : ` · ${card.deletion.uncovered} targets or conditions cannot be recovered`}</p>}
    {detail && <p role="alert">{detail}</p>}
    {error && <p role="alert">{error}</p>}
    {replying && <div className="approval-reply">
      <p id={helpId}>{card.replyMode === 'feedback' ? 'declines this step and sends your words' : 'sends after stopping the current turn'}</p>
      <textarea aria-label={`Reply to ${label} approval`} aria-describedby={helpId} autoFocus value={text} rows={2} disabled={answering || stopping || !!draft.pending} onChange={event => draft.setText(event.target.value)} onKeyDown={event => {
        if (event.key === 'Escape' && !event.nativeEvent.isComposing && !answering) { event.preventDefault(); cancelReply(); }
        if (event.metaKey && event.key === 'Enter' && !event.nativeEvent.isComposing && validReply && !stopping) { event.preventDefault(); void decide({ action: 'reply', text, mode: card.replyMode === 'feedback' ? 'feedback' : 'restart' }); }
      }} />
    </div>}
    <div data-card-actions>{replying ? <>
      <button disabled={answering || stopping} onClick={cancelReply}>Cancel reply</button>
      <button disabled={!validReply || answering || stopping} onClick={() => { void decide({ action: 'reply', text, mode: card.replyMode === 'feedback' ? 'feedback' : 'restart' }); }}>{card.replyMode === 'feedback' ? 'Decline and reply' : 'Stop and send'}</button>
    </> : <>
      {card.canAllow && <button disabled={!read || answering || stopping} onClick={() => { void decide({ action: 'allow' }); }}>Allow once</button>}
      <button disabled={!card.canDeny || answering || stopping} onClick={() => { void decide({ action: 'deny' }); }}>Don’t run it</button>
      {draftContext && <button ref={replyButton} disabled={answering || stopping || !draft.ready} onClick={() => { dismissed.current = false; setReplying(true); }}>Reply</button>}
    </>}</div>
  </div>;
}
