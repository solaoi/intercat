import { useContext, useEffect, useRef, useState } from 'react';
import type { BranchPoint, BranchRequest, BranchState, LiveTrack } from '../bridge';
import { AttentionActive } from '../desk/NeedsYou';
import { usePrivacyLabels } from '../desk/PrivacyText';
import './branch.css';

export interface BranchSelection { operationId: string; trackId: string | null; at: number | null }

export function BranchDialog({ selection, tracks, connected, blockedReason, close, created }: {
  selection: BranchSelection; tracks: LiveTrack[]; connected: boolean; blockedReason: string | null;
  close: () => void; created: (branch: BranchState) => void;
}) {
  const label = usePrivacyLabels(tracks.map(track => track.title));
  const dialog = useRef<HTMLDialogElement>(null);
  const [trackId, setTrack] = useState(selection.trackId ?? '');
  const [query, setQuery] = useState({ trackId: selection.trackId ?? '', before: null as number | null, attempt: 0 });
  const [points, setPoints] = useState<BranchPoint[]>([]);
  const [before, setBefore] = useState<number | null>(null);
  const [pointId, setPoint] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [pending, setPending] = useState<BranchRequest | null>(null);
  const mounted = useRef(true);
  const track = tracks.find(track => track.id === trackId);
  const point = points.find(point => point.id === pointId);

  useEffect(() => {
    mounted.current = true;
    dialog.current?.showModal();
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    if (!query.trackId || !connected) { setLoading(false); return; }
    let disposed = false;
    setLoading(true); setError('');
    void window.intercat.getBranchPoints({ trackId: query.trackId, before: query.before }).then(page => {
      if (disposed) return;
      setPoints(previous => query.before === null ? page.points : [...previous, ...page.points.filter(point => !previous.some(old => old.id === point.id))]);
      setBefore(page.before);
      if (query.before === null && selection.at !== null) {
        const closest = page.points.find(point => point.available && point.timelineAt !== null && point.timelineAt <= selection.at!);
        setPoint(closest?.id ?? null);
      }
    }).catch(failure => { if (!disposed) setError(failure instanceof Error ? failure.message : 'Saved branch points could not be loaded.'); })
      .finally(() => { if (!disposed) setLoading(false); });
    return () => { disposed = true; };
  }, [query, connected, selection.at]);

  async function branch() {
    if (!track || !point?.available || !connected || blockedReason || busy) return;
    const request = pending ?? { opId: selection.operationId, parentTrackId: trackId, turnId: point.turnId };
    setPending(request); setBusy(true); setError('');
    try {
      const result = await window.intercat.branchFromCheckpoint(request);
      if (!mounted.current) return;
      if (result.phase !== 'ready') throw new Error('The branch is still being prepared. Retry to finish the same branch.');
      created(result);
    } catch (failure) {
      if (mounted.current) setError(failure instanceof Error ? failure.message : 'The branch could not be prepared.');
    } finally { if (mounted.current) setBusy(false); }
  }

  return <dialog ref={dialog} className="branch-dialog" aria-labelledby="branch-title" onCancel={event => { event.preventDefault(); if (!busy) close(); }}>
    <header><h2 id="branch-title">Branch from a saved turn</h2><button aria-label="Close branch preparation" disabled={busy} onClick={close}>×</button></header>
    <div className="branch-dialog-body">
      <p>Choose a completed turn. Nothing branches until you confirm.</p>
      {selection.at !== null && <p>Check the saved turn below against the step you selected.</p>}
      <label>Worker<select aria-label="Branch source worker" value={trackId} disabled={!!pending || busy} onChange={event => {
        const next = event.target.value; setTrack(next); setPoints([]); setPoint(null); setBefore(null); setError('');
        setQuery({ trackId: next, before: null, attempt: 0 });
      }}><option value="">Choose a worker</option>{tracks.map(track => <option value={track.id} key={track.id}>{track.label} · {label(track.title)}</option>)}</select></label>
      {!!trackId && !track && <p role="alert">This worker is no longer available.</p>}
      <fieldset disabled={!!pending || busy || !connected} className="branch-points"><legend>Saved turns</legend>
        {points.map(candidate => <label key={candidate.id} className={candidate.available ? '' : 'unavailable'}>
          <input type="radio" name="branch-point" checked={pointId === candidate.id} disabled={!candidate.available} onChange={() => setPoint(candidate.id)} />
          <span><strong>{candidate.endedAt === null ? 'Saved turn · time unavailable' : new Date(candidate.endedAt).toLocaleString()}</strong>
            <small>End of {track?.label ?? 'this worker'}’s turn · {candidate.checkpoint.slice(0, 10)}{candidate.shellWasRunning ? ' · shell was running' : ''}</small>
            {!candidate.available && <small>This checkpoint is not available for branching.</small>}
          </span>
        </label>)}
        {!loading && !error && connected && track && !points.length && <p>no checkpoint yet</p>}
      </fieldset>
      {loading && <p role="status">Loading saved turns…</p>}
      {!connected && <p>Reconnect to load saved turns.</p>}
      {before !== null && <button disabled={loading || !!pending} onClick={() => setQuery({ trackId, before, attempt: query.attempt + 1 })}>Load older turns</button>}
      <p className="branch-scope">Covers: tracked, untracked · not: ignored.<br />Uses instructions saved at the checkpoint. Keeps the original track. Effects outside Git stay as they are.</p>
      {blockedReason && <p>{blockedReason}</p>}
      {error && <div className="branch-error" role="alert"><p>{error}</p>{!pending && <button disabled={loading} onClick={() => setQuery(previous => ({ ...previous, attempt: previous.attempt + 1 }))}>Retry loading turns</button>}</div>}
    </div>
    <footer><span>{point ? `Branch from ${point.endedAt === null ? point.checkpoint.slice(0, 10) : new Date(point.endedAt).toLocaleString()}` : 'Choose a saved turn'}</span><button disabled={busy || loading || !connected || !!blockedReason || !track || !point?.available} onClick={() => { void branch(); }}>{busy ? 'Preparing branch…' : pending ? 'Retry branch' : 'Branch from here'}</button></footer>
  </dialog>;
}

export function BranchRecovery({ branch, connected, created }: { branch: BranchState; connected: boolean; created: (branch: BranchState) => void }) {
  const active = useContext(AttentionActive);
  const mounted = useRef(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  return <div className="desk-card desk-action-card">
    <div className="desk-copy"><strong>Finish preparing this branch</strong><p>The saved turn and destination are already recorded. Retrying continues the same branch.</p>{error && <p role="alert">{error}</p>}</div>
    <div className="desk-actions" data-card-actions><button disabled={busy || !connected || !active} onClick={async () => {
      setBusy(true); setError('');
      try {
        const result = await window.intercat.branchFromCheckpoint(branch.request);
        if (!mounted.current) return;
        if (result.phase !== 'ready') throw new Error('The branch is still being prepared. Retry to finish the same branch.');
        created(result);
      }
      catch (failure) { if (mounted.current) setError(failure instanceof Error ? failure.message : 'The branch could not be prepared.'); }
      finally { if (mounted.current) setBusy(false); }
    }}>{busy ? 'Preparing…' : 'Retry branch preparation'}</button></div>
  </div>;
}
