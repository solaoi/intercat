import { useRef, useState } from 'react';
import type { ReviewNote, ReviewSummary } from '../bridge';
export function ReviewNotes({ reviews = [], notes = [], refresh }: { reviews?: ReviewSummary[]; notes?: ReviewNote[]; refresh: () => void }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState('');
  const pendingStop=useRef<{opId:string;reviewId:string;revision:number}|null>(null);
  const pendingStart = useRef<{ opId: string; utteranceId: string; revision: number } | null>(null);
  const pending = useRef<{ opId: string; reviewId: string; revision: number } | null>(null);
  if (!reviews.length && !notes.length) return null;
  async function start(note: ReviewNote) {
    if (busy || !note.fix?.inputId || note.fix.inputRevision == null) return;
    setBusy(note.id); setError('');
    try {
      if (!pendingStart.current || pendingStart.current.utteranceId !== note.fix.inputId || pendingStart.current.revision !== note.fix.inputRevision)
        pendingStart.current = {opId: await window.intercat.createOperationId(), utteranceId: note.fix.inputId, revision: note.fix.inputRevision};
      await window.intercat.startHeldUtterance(pendingStart.current); pendingStart.current=null; refresh();
    } catch(error) {setError(error instanceof Error ? error.message : 'The fix could not be started.');}
    finally {setBusy(null);}
  }
  async function stop(review:ReviewSummary) {
    if(busy)return;setBusy(review.id);setError('');
    try {
      if(!pendingStop.current||pendingStop.current.reviewId!==review.id||pendingStop.current.revision!==review.revision)pendingStop.current={opId:await window.intercat.createOperationId(),reviewId:review.id,revision:review.revision};
      await window.intercat.stopReview(pendingStop.current);pendingStop.current=null;refresh();
    } catch(error){setError(error instanceof Error?error.message:'The review could not be stopped.');}
    finally{setBusy(null);}
  }
  async function retry(review: ReviewSummary) {
    if (busy) return;
    setBusy(review.id); setError('');
    try {
      if (!pending.current || pending.current.reviewId !== review.id || pending.current.revision !== review.revision)
        pending.current = { opId: await window.intercat.createOperationId(), reviewId: review.id, revision: review.revision };
      await window.intercat.retryReview(pending.current);
      pending.current = null; refresh();
    } catch (error) { setError(error instanceof Error ? error.message : 'The review could not be retried.'); }
    finally { setBusy(null); }
  }
  return <section className="review-notes" aria-label="Reviewer notes">
    <h3>Reviewer</h3>
    {reviews.map(review => <div key={review.id} className="review-status">
      {review.state === 'complete' ? <p>{review.summary}</p> : <p>{review.state === 'queued' ? 'Waiting to review' : review.state === 'running' ? 'Reviewing saved checkpoint…' : review.detail ?? 'Review did not finish.'}</p>}
      {(review.state==='queued'||review.state==='running') && <button disabled={busy!==null} onClick={()=>{void stop(review);}}>Stop review</button>}
      {(review.state === 'held' || review.state === 'failed') && review.retryable !== false && <button disabled={busy !== null} onClick={() => { void retry(review); }}>{busy === review.id ? 'Retrying…' : 'Retry review'}</button>}
    </div>)}
    {notes.map(note => <article className="review-note" key={note.id}>
      <p><strong>{note.kind} · {note.blocking ? 'required' : 'optional'}</strong>{note.outdated && <span> · may be outdated</span>}</p>
      <p className="review-location">{note.path}:{note.startLine}{note.endLine !== note.startLine ? `–${note.endLine}` : ''}</p>
      <p>{note.text}</p>
      {note.fix && <p>{({reserved:'Fix starting',running:'Fix running',resolved:'Resolved after review',unresolved:'Still present after review',kept:'kept as is',superseded:'Replaced by a new instruction','awaiting-review':'Waiting for re-review',blocked:'Waiting for your decision',outdated:'may be outdated',cut:'Fix taken back',held:'Fix held'} as Partial<Record<typeof note.fix.state,string>>)[note.fix.state] ?? 'Fix queued'}{note.fix.detail ? ` · ${note.fix.detail}` : ''}</p>}
      {note.fix?.state === 'held' && <button disabled={busy!==null} onClick={()=>{void start(note);}}>Start fix</button>}
    </article>)}
    {error && <p role="alert">{error}</p>}
  </section>;
}
