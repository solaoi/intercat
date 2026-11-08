import { useRef, useState } from 'react';
import type { Proposal, ProposalAction, ProposalCommand, ProposalResponse } from '../bridge';
import type { AttentionCard } from './NeedsYou';
import { usePrivacyLabels } from './PrivacyText';

type PlanProposal = Extract<Proposal, { kind: 'plan-conflict' }>;
function ProposalCard({ proposal, disabled, answered, reply }: { proposal: Proposal; disabled: boolean; answered: (response: ProposalResponse) => void; reply: (proposal: PlanProposal) => void }) {
  const replyReason = proposal.kind === 'plan-conflict' ? proposal.replyBlockedReason : null;
  const label = usePrivacyLabels([proposal.text, proposal.detail, ...proposal.options.map(option => option.disabledReason ?? ''), replyReason ?? '']);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<ProposalCommand | null>(null);
  async function choose(action: ProposalAction) {
    setBusy(true); setError('');
    try {
      const command = pending.current ?? { opId: await window.intercat.createOperationId(), proposalId: proposal.id, action };
      pending.current = command;
      const response = await window.intercat.answerProposal(command);
      pending.current = null;
      answered(response);
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : 'The choice could not be saved.');
    } finally { setBusy(false); }
  }
  return <div className="desk-card desk-action-card proposal-card">
    <div className="desk-copy">
      <strong>{label(proposal.text)}</strong>
      <details><summary>Details</summary><p>{label(proposal.detail)}</p></details>
      {proposal.options.map((option, index) => option.disabledReason && <p key={option.action} id={`proposal-${proposal.id}-reason-${index}`}>{label(option.disabledReason)}</p>)}
      {replyReason && <p id={`proposal-${proposal.id}-reply-reason`}>{label(replyReason)}</p>}
      {error && <p role="alert">{error}</p>}
    </div>
    <div className="desk-actions" data-card-actions>
      {pending.current ? <button disabled={disabled || busy} onClick={() => { void choose(pending.current!.action); }}>Retry saving choice</button>
        : proposal.options.map((option, index) => <button key={option.action} disabled={disabled || busy || !!option.disabledReason} aria-describedby={option.disabledReason ? `proposal-${proposal.id}-reason-${index}` : undefined} onClick={() => { void choose(option.action); }}>{option.text}</button>)}
      {proposal.kind === 'plan-conflict' && !pending.current && <button disabled={disabled || busy || !!replyReason} aria-describedby={replyReason ? `proposal-${proposal.id}-reply-reason` : undefined} onClick={() => reply(proposal)}>Reply…</button>}
    </div>
  </div>;
}

export function proposalCards(proposal: Proposal | null | undefined, disabled: boolean, answered: (response: ProposalResponse) => void, reply: (proposal: PlanProposal) => void): AttentionCard[] {
  return proposal ? [{ id: `proposal-${proposal.id}`, kind: 'proposal', title: proposal.text, version: `${proposal.text}\n${proposal.detail}`,
    collapseAt: proposal.createdAt + 10 * 60 * 1000, createdAt: proposal.createdAt,
    content: <ProposalCard key={proposal.id} proposal={proposal} disabled={disabled} answered={answered} reply={reply} /> }] : [];
}
