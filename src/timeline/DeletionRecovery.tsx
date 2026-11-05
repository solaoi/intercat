import { useRef, useState } from 'react';
import type { DeletionSnapshot } from '../bridge';

export function DeletionRecovery({ snapshot }: { snapshot: DeletionSnapshot }) {
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const operation = useRef<string | null>(snapshot.restoreOperationId ?? null);
  async function restore() {
    setBusy(true); setMessage('');
    try {
      operation.current ??= await window.intercat.createOperationId();
      const result = await window.intercat.restoreDeletion({ opId: operation.current, trackId: snapshot.trackId, repairRunId: snapshot.repairRunId ?? null, snapshotId: snapshot.id });
      setMessage(`Restored ${result.files} saved files. The ${snapshot.repairRunId ? 'repair' : 'worker'} is held.`); setConfirm(false); operation.current = null;
    } catch (error) { setMessage(`Restore did not finish: ${error instanceof Error ? error.message : String(error)}. You can retry this restore.`); }
    finally { setBusy(false); }
  }
  return <section className="deletion-recovery" aria-label={snapshot.purpose === 'before-delete' ? 'Before deletion' : 'Before restore'}>
    <strong>{snapshot.purpose === 'before-delete' ? 'Before deletion' : 'Before restore'} · {snapshot.paths.length} files saved</strong>
    <small>{snapshot.sha.slice(0, 12)} · {snapshot.complete ? 'All identified targets covered' : `${snapshot.uncovered.length} targets or conditions not covered`}</small>
    {snapshot.restoreOperationId && <p>An unfinished restore is waiting. Retry it to finish before starting new work.</p>}
    <details><summary>Saved paths</summary><pre>{[...snapshot.paths, ...snapshot.absent.map(path => `${path} (absent)`)].join('\n')}</pre>{snapshot.uncovered.length > 0 && <p>{snapshot.uncovered.join('\n')}</p>}</details>
    {confirm ? <div><p>This replaces the saved paths with this snapshot. Current contents are backed up first. Stop the {snapshot.repairRunId ? 'repair' : 'worker'} before restoring.</p><button disabled={busy} onClick={() => { void restore(); }}>{busy ? 'Restoring…' : 'Restore now'}</button><button disabled={busy} onClick={() => setConfirm(false)}>Cancel</button></div> : <button onClick={() => { setConfirm(true); setMessage(''); }}>Restore saved files</button>}
    {message && <p role="status">{message}</p>}
  </section>;
}
