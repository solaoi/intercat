import { useEffect, useRef, useState } from 'react';
import type { Approval } from '../bridge';

export function ApprovalCard({ stopping, detail, label, card, answer }: { stopping: boolean; detail: string | null; label: string; card: Approval; answer: (card: Approval, allow: boolean) => Promise<void> }) {
  const subject = useRef<HTMLPreElement>(null);
  const [read, setRead] = useState(false);
  const [answering, setAnswering] = useState(false);
  async function decide(allow: boolean) { setAnswering(true); try { await answer(card, allow); } finally { setAnswering(false); } }
  useEffect(() => { const element = subject.current; setRead(!!element && element.scrollHeight <= element.clientHeight + 1); }, [card.id, card.subject, JSON.stringify(card.requirement)]);
  return <div className="approval-card" role="group" aria-label={`Approval for ${label}`}>
    <strong>{label} · {card.reason.message}</strong>
    <pre ref={subject} tabIndex={0} onScroll={event => { const element = event.currentTarget; if (element.scrollTop + element.clientHeight >= element.scrollHeight - 1) setRead(true); }}>{card.subject}</pre>
    {card.deletion && <p>Snapshot {card.deletion.sha.slice(0, 8)} saved · {card.deletion.files} files covered{card.deletion.complete ? '' : ` · ${card.deletion.uncovered} targets or conditions cannot be recovered`}</p>}
    {detail && <p role="alert">{detail}</p>}
    <div>{card.canAllow && <button disabled={!read || answering || stopping} onClick={() => { void decide(true); }}>Allow once</button>}<button disabled={!card.canDeny || answering || stopping} onClick={() => { void decide(false); }}>Don’t run it</button></div>
  </div>;
}
