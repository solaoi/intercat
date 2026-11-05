import { useRef, useState } from 'react';
import type { WorkerState, PickupState } from '../bridge';

export function PickupButton({ worker, pending }: { worker: WorkerState; pending?: PickupState }) {
  const [operation, setOperation] = useState<string | null>(null);
  const cancelOperation = useRef<string | null>(null);
  const cancelAttempted = useRef(false);
  const [busy, setBusy] = useState(false);
  const [cancelState, setCancelState] = useState<'idle' | 'running' | 'failed' | 'done'>('idle');
  const [error, setError] = useState('');
  const active = pending && !['ready', 'cancelled'].includes(pending.phase);
  const recoveryId = active ? pending.opId : operation;
  const cancelling = cancelState === 'running' || pending?.phase === 'cancelling';
  const cancelled = cancelState === 'done' || pending?.phase === 'cancelled';
  async function recover() {
    if (!worker.sessionId || busy || cancelling) return;
    setBusy(true); setError(''); setCancelState('idle'); cancelAttempted.current = false; cancelOperation.current = null;
    try {
      const opId = (active ? pending.opId : operation) ?? await window.intercat.createOperationId();
      setOperation(opId);
      await window.intercat.pickupFromFiles({ opId, trackId: worker.trackId, sessionId: worker.sessionId });
    } catch (error) {
      if (!cancelAttempted.current) setError(error instanceof Error ? error.message : 'File recovery could not finish. Retry to continue from its saved step.');
    } finally { setBusy(false); }
  }
  async function cancel() {
    if (!worker.sessionId || !recoveryId || cancelState === 'running') return;
    setOperation(recoveryId);
    cancelAttempted.current = true; setCancelState('running'); setError('');
    try {
      const opId = cancelOperation.current ?? await window.intercat.createOperationId();
      cancelOperation.current = opId;
      await window.intercat.cancelFileRecovery({ opId, recoveryId, trackId: worker.trackId, sessionId: worker.sessionId });
      setOperation(null); setCancelState('done');
    } catch (error) {
      setCancelState('failed');
      setError(error instanceof Error ? error.message : 'Cancellation could not finish. Retry cancellation.');
    }
  }
  return <div className="pickup-actions">
    <button disabled={busy || cancelling || cancelState === 'failed' || !worker.sessionId} onClick={() => { void recover(); }}>{cancelling ? 'Cancelling recovery…' : busy ? pending?.phase === 'summarizing' ? 'Summarizing history…' : 'Recovering files…' : active ? 'Retry file recovery' : 'Pick up from files'}</button>
    {recoveryId && (active || busy || cancelState === 'failed') && (!cancelled || cancelState === 'failed') && <button disabled={cancelState === 'running'} onClick={() => { void cancel(); }}>{cancelState === 'failed' || pending?.phase === 'cancelling' ? 'Retry cancellation' : 'Cancel recovery'}</button>}
    {cancelled && <p>Recovery cancelled. Files and saved snapshots were kept.</p>}
    {error && <p>{error}</p>}
  </div>;
}
