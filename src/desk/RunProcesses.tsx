import { useEffect, useRef, useState } from 'react';
import type { RepairRun, RemainingRunProcesses, RunProcessCommand } from '../commit/repair';
import type { AttentionCard } from './NeedsYou';
import { PrivacyText } from './PrivacyText';

type Command = (request: RunProcessCommand) => Promise<void>;
function Processes({ run, remaining, disabled, command }: { run: RepairRun; remaining: RemainingRunProcesses; disabled: boolean; command: Command }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<RunProcessCommand | null>(null);
  useEffect(() => {
    const request = pending.current;
    if (busy || !request || request.nonce !== remaining.nonce || remaining.revision <= request.revision) return;
    if (request.action === 'keep' && remaining.state === 'kept' || request.action === 'stop' && ['stopping', 'cleaning'].includes(remaining.state) || request.action === 'retry-cleanup' && remaining.state === 'cleaning') {
      pending.current = null; setError('');
    }
  }, [busy, error, remaining.nonce, remaining.revision, remaining.state]);
  async function decide(action: RunProcessCommand['action']) {
    if (disabled || busy) return;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), id: run.id, nonce: remaining.nonce, revision: remaining.revision, action };
      await command(pending.current);
      pending.current = null;
    } catch (failure) {
      setError((failure instanceof Error ? failure.message : 'The process decision could not be confirmed.').replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/, ''));
    } finally { setBusy(false); }
  }
  const stopping = remaining.state === 'stopping';
  const cleaning = remaining.state === 'cleaning';
  return <div className="run-processes">
    <strong>{cleaning ? `Worker ${remaining.label}’s run needs attention` : remaining.error ? `Worker ${remaining.label}’s processes need checking` : `Worker ${remaining.label} still has running processes`}</strong>
    <p>Left by {remaining.label}’s {run.purpose.kind === 'merge' ? 'merge' : 'fix'} run · <PrivacyText text={remaining.sessionName} /></p>
    <p>{cleaning ? 'Its files are being kept.' : remaining.state === 'kept' ? 'Kept running.' : stopping ? 'Stopping the remaining processes…' : ['captured', 'applying'].includes(run.phase) ? 'The turn has ended. Its result is being saved.' : 'The turn has ended.'}</p>
    {(error || remaining.error) && <p role="alert"><PrivacyText text={error || remaining.error!} /></p>}
    <div className="run-process-actions" data-card-actions>
      {error && pending.current && <button disabled={disabled || busy} onClick={() => { pending.current = null; setError(''); }}>Review current processes</button>}
      {cleaning ? <button disabled={disabled || busy} onClick={() => { void decide('retry-cleanup'); }}>Retry cleanup</button> : <><button disabled={disabled || busy || remaining.state === 'kept' || stopping || !!pending.current && pending.current.action !== 'keep'} onClick={() => { void decide('keep'); }}>{pending.current?.action === 'keep' ? 'Retry Keep' : 'Keep'}</button>
      <button disabled={disabled || busy || stopping && !remaining.error || !!pending.current && pending.current.action !== 'stop'} onClick={() => { void decide('stop'); }}>{pending.current?.action === 'stop' ? 'Retry Stop' : stopping ? remaining.error ? 'Try Stop again' : 'Stopping…' : 'Stop it'}</button></>}
    </div>
  </div>;
}
export function runProcessCards(runs: RepairRun[], disabled: boolean, command: Command): AttentionCard[] {
  return runs.flatMap(run => {
    const remaining = run.remaining;
    if (!remaining || remaining.state === 'cleaning' && !remaining.error) return [];
    return [{ id: `run-processes-${run.id}`, version: `${remaining.nonce}:${remaining.revision}`, kind: remaining.error ? 'blocked' as const : 'proposal' as const,
      createdAt: remaining.createdAt, title: remaining.error ? `Worker ${remaining.label}’s run needs attention` : `Worker ${remaining.label} still has running processes`,
      content: <Processes key={`${run.id}:${remaining.nonce}`} run={run} remaining={remaining} disabled={disabled} command={command} /> }];
  });
}
