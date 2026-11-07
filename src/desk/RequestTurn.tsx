import { lazy, Suspense, useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { RequestOrigin, RequestTurnChanges, RequestTurnIdentity } from '../bridge';
import type { Diff } from '../commit/types';

const DiffView = lazy(() => import('../commit/DiffView'));

function TurnChanges({ identity, close }: { identity: RequestTurnIdentity; close: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [changes, setChanges] = useState<RequestTurnChanges | null>(null);
  const [selected, setSelected] = useState('');
  const [diff, setDiff] = useState<Diff | null>(null);
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  useLayoutEffect(() => {
    const element = dialog.current!;
    const previous = document.activeElement as HTMLElement | null;
    element.showModal();
    return () => { element.close(); if (previous?.isConnected) previous.focus(); };
  }, []);
  useEffect(() => {
    let current = true;
    setError(''); setChanges(null); setDiff(null);
    void window.intercat.getRequestChanges(identity).then(value => {
      if (current) { setChanges(value); setSelected(value.files[0] ?? ''); }
    }).catch(error => { if (current) setError(error instanceof Error ? error.message : 'The saved changes could not be read.'); });
    return () => { current = false; };
  }, [identity.utteranceId, identity.sessionId, identity.turnId, attempt]);
  useEffect(() => {
    let current = true;
    setDiff(null);
    if (changes && selected) void window.intercat.getRequestDiff({ identity, checkpoint: changes.checkpoint, path: selected }).then(value => {
      if (current) { setDiff(value); setError(''); }
    }).catch(error => { if (current) setError(error instanceof Error ? error.message : 'The file could not be read.'); });
    return () => { current = false; };
  }, [changes, selected, identity.utteranceId, identity.sessionId, identity.turnId]);
  return <dialog ref={dialog} className="turn-changes" aria-labelledby="turn-changes-title" onCancel={event => { event.preventDefault(); close(); }} onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); close(); } }}>
    <header><h2 id="turn-changes-title">{changes ? `${changes.label}’s changes` : 'Turn changes'}</h2><button onClick={close} aria-label="Close turn changes">×</button></header>
    {error && <div role="alert">{error}<button onClick={() => setAttempt(value => value + 1)}>Retry saved changes</button></div>}
    {!changes && !error && <p role="status">Reading saved changes…</p>}
    {changes && <><p className="turn-checkpoint">Saved turn · {changes.checkpoint.slice(0, 12)}</p>
      {changes.files.length === 0 ? <p>No file changes in this turn.</p> : <div className="turn-change-content">
        <nav aria-label="Changed files">{changes.files.map(path => <button key={path} aria-pressed={selected === path} onClick={() => setSelected(path)}>{path}</button>)}</nav>
        <section aria-label="Saved file changes">{diff ? <><Suspense fallback={<p>Opening diff…</p>}><DiffView value={diff} /></Suspense>{diff.redacted && <p role="note">Secrets are hidden in this preview.</p>}</> : !error && <p role="status">Reading file…</p>}</section>
      </div>}</>}
  </dialog>;
}

export function RequestTurn({ input }: { input: string }) {
  const [origin, setOrigin] = useState<RequestOrigin | null>(null);
  const [readingError, setReadingError] = useState('');
  const [actionError, setActionError] = useState('');
  const [busy, setBusy] = useState(false);
  const actionPending = useRef(false);
  const [showChanges, setShowChanges] = useState<RequestTurnIdentity | null>(null);
  const refresh = useRef<() => Promise<void>>(async () => {});
  const mounted = useRef(true);
  useEffect(() => {
    let pending = false;
    let active = true;
    mounted.current = true;
    const read = async () => {
      if (pending) return;
      pending = true;
      try {
        const value = await window.intercat.getRequestOrigin(input);
        if (active) { setOrigin(value); setReadingError(''); }
      } catch (error) {
        if (active) { setOrigin(null); setReadingError(error instanceof Error ? error.message : 'The original turn could not be read.'); }
      } finally { pending = false; }
    };
    refresh.current = read;
    void read();
    const timer = setInterval(() => { void read(); }, 1000);
    return () => { active = false; mounted.current = false; clearInterval(timer); };
  }, [input]);
  async function stop() {
    if (!origin || actionPending.current) return;
    actionPending.current = true;
    setBusy(true); setActionError('');
    try {
      await window.intercat.stopRequestTurn({ opId: await window.intercat.createOperationId(), trackId: origin.trackId, identity: origin.identity });
      await refresh.current();
    } catch (error) { if (mounted.current) setActionError(error instanceof Error ? error.message : 'The original turn could not be stopped.'); }
    finally { actionPending.current = false; if (mounted.current) setBusy(false); }
  }
  if (!origin && !readingError) return null;
  return <section className="request-turn" aria-label="Turn started by this request">
    {origin && <div className="request-turn-action"><span>{origin.phase === 'moved-on' ? `${origin.label} has moved on` : origin.phase === 'stopping' ? `Stopping ${origin.label}…` : origin.phase === 'running' ? `${origin.label} is running this request` : origin.phase === 'saving' ? `Saving ${origin.label}’s changes` : origin.phase === 'finished' ? `${origin.label} finished this request` : 'The original turn is unavailable'}</span>
      {origin.phase === 'running' && <button disabled={busy} onClick={() => { void stop(); }}>Stop {origin.label}</button>}
      {origin.checkpoint && <button disabled={busy} onClick={() => setShowChanges(origin.identity)}>Show {origin.label}’s changes</button>}
    </div>}
    {readingError && <p role="status">{readingError}<button onClick={() => { void refresh.current(); }}>Retry turn details</button></p>}
    {actionError && <p role="alert">{actionError}</p>}
    {showChanges && <TurnChanges identity={showChanges} close={() => setShowChanges(null)} />}
  </section>;
}
