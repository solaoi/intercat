import type { ContinuationContext as Context } from '../desk/drafts';
import { PrivacySummary } from '../desk/PrivacyText';

export function ContinuationContext({ context, label, stale, busy, pending, review, close }: {
  context: Context; label: string; stale: boolean; busy: boolean; pending: boolean; review: () => void; close: () => void;
}) {
  return <div className="reply-target" aria-label="Plan item for this instruction">
    <span>about · <PrivacySummary text={context.title} limit={60} /> · Worker {label}</span>
    <span>after the current turn</span>
    <button disabled={busy} onClick={close}>Back to draft</button>
    {stale && !pending && <span role="status">The plan changed · <button disabled={busy} onClick={review}>Review item</button></span>}
  </div>;
}
