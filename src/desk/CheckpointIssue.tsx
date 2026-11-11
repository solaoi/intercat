import { useId, useRef, useState } from 'react';
import type { CheckpointIssue, DaemonStatus } from '../bridge';
import type { AttentionCard } from './NeedsYou';
import { PrivacyText } from './PrivacyText';

function IssueCard({ issue }: { issue: CheckpointIssue }) {
  const id = useId();
  const operation = useRef<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [stopping, setStopping] = useState(false);
  const [error, setError] = useState('');
  function failure(error: unknown) {
    setError((error instanceof Error ? error.message : String(error)).replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/, ''));
  }
  async function exclude() {
    setBusy(true); setError('');
    try {
      operation.current ??= await window.intercat.createOperationId();
      await window.intercat.excludeCheckpointRepository({ opId: operation.current, issueId: issue.id, revision: issue.revision });
    } catch (error) { failure(error); }
    finally { setBusy(false); }
  }
  async function stop() {
    setStopping(true); setError('');
    try {
      await window.intercat.stopWorker({ opId: await window.intercat.createOperationId(), trackId: issue.trackId, turnId: null });
    } catch (error) { failure(error); }
    finally { setStopping(false); }
  }
  return <div className="inheritance-recovery checkpoint-recovery" role="group" aria-labelledby={`${id}-heading`} aria-describedby={`${id}-scope`}>
    <strong id={`${id}-heading`}><PrivacyText text={`${issue.path} is a separate repository · its files can’t be saved`} /></strong>
    <p id={`${id}-scope`}>Exclude this folder from this track’s checkpoints to continue saving. Its files stay where they are.</p>
    {error && <p role="alert"><PrivacyText text={error} /></p>}
    <div className="inheritance-actions" data-card-actions>
      <button disabled={busy} aria-describedby={`${id}-heading ${id}-scope`} onClick={() => { void exclude(); }}>Exclude it</button>
      <button disabled={stopping} aria-describedby={`${id}-heading ${id}-scope`} onClick={() => { void stop(); }}>Stop</button>
    </div>
  </div>;
}

export function checkpointIssueCards(status: DaemonStatus): AttentionCard[] {
  return (status.checkpointIssues ?? []).map(issue => {
    const label = status.tracks?.find(track => track.id === issue.trackId)?.label ?? '';
    return { id: `checkpoint-issue-${issue.id}`, version: issue.revision, kind: 'stopped',
      title: `Checkpoint for Worker ${label}`, destination: { trackId: issue.trackId, label },
      content: <IssueCard key={`${issue.id}:${issue.revision}`} issue={issue} /> };
  });
}
