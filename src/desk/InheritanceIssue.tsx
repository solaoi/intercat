import { useState } from 'react';
import type { InheritanceIssue, LiveTrack, WorkerState } from '../bridge';
import type { AttentionCard } from './NeedsYou';
import { PrivacyText } from './PrivacyText';

type Props = {
  issues: InheritanceIssue[];
  workers: WorkerState[];
  tracks: LiveTrack[];
  command: (issue: InheritanceIssue, action: 'keep' | 'stop') => Promise<void>;
  finish: (issue: InheritanceIssue) => Promise<void>;
  review: (issue: InheritanceIssue) => void;
};

function IssueCard({ issue, worker, label, parents, command, finish, review }: Omit<Props, 'issues' | 'workers' | 'tracks'> & { issue: InheritanceIssue; worker?: WorkerState; label: string; parents: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const processes = issue.blocker.kind === 'processes';
  const canStop = worker && worker.sessionId === issue.sessionId && !worker.activeTurn && !worker.checkpointPending && ['ready', 'held'].includes(worker.phase);
  function run(task: () => Promise<void>) {
    setBusy(true); setError('');
    void task().catch(error => setError((error instanceof Error ? error.message : String(error)).replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/, ''))).finally(() => setBusy(false));
  }
  return <div className="inheritance-recovery">
    <strong>{processes ? `Worker ${label} still has running processes` : `Changes for Worker ${label} are on hold`}</strong>
    {processes ? <p>{issue.kept ? `Kept running · ${parents} wait.` : `${parents} cannot be applied while these processes are running.`}</p>
      : issue.blocker.kind === 'conflict' ? <><p>No inherited files were written. Review the dependencies before retrying.</p><ul className="inheritance-paths">{issue.blocker.paths.map(path => <li key={path}><PrivacyText text={path} /></li>)}</ul></>
        : issue.blocker.kind === 'other' && <p><PrivacyText text={issue.blocker.message} /></p>}
    {processes && <p>{issue.applicationId ? 'After stopping, finish the saved file update. No request will be sent.' : 'After stopping, use Start to continue this request.'}</p>}
    {!processes && issue.applicationId && <p>Finishing the saved update will not send a request.</p>}
    {error && <p role="alert"><PrivacyText text={error} /></p>}
    <div className="inheritance-actions" data-card-actions>
      {issue.planId && <button disabled={busy} onClick={() => review(issue)}>Review plan</button>}
      <button disabled={busy || issue.kept} onClick={() => run(() => command(issue, 'keep'))}>{processes ? `Keep (${parents} wait)` : 'Keep on hold'}</button>
      {processes ? <button disabled={busy || !canStop} onClick={() => run(() => command(issue, 'stop'))}>{busy ? 'Stopping…' : 'Stop it'}</button>
        : issue.applicationId && <button disabled={busy || !canStop} onClick={() => run(() => finish(issue))}>Finish file update</button>}
    </div>
  </div>;
}

export function inheritanceIssueCards(props: Props): AttentionCard[] {
  return props.issues.map(issue => {
    const label = props.tracks.find(track => track.id === issue.trackId)?.label ?? '';
    const parentLabels = issue.parents.map(id => props.tracks.find(track => track.id === id)?.label).filter(Boolean);
    const parents = parentLabels.length ? `${parentLabels.join(' + ')}’s changes` : 'Dependency changes';
    return { id: `inheritance-issue-${issue.id}`, version: issue.id, kind: issue.kept ? 'blocked' : 'stopped', title: `Changes for Worker ${label}`, destination: { trackId: issue.trackId, label }, content: <IssueCard key={issue.id} issue={issue} label={label} parents={parents} worker={props.workers.find(worker => worker.trackId === issue.trackId)} command={props.command} finish={props.finish} review={props.review} /> };
  });
}
