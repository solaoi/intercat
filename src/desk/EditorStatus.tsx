import { createPortal } from 'react-dom';
import { useContext, useEffect, useRef, useState } from 'react';
import type { EditorStatus as Status } from '../bridge';
import './editor-status.css';
import { AttentionActive } from './NeedsYou';

export function EditorStatus({ value, update }: { value: Status; update: (value: Status) => void }) {
  const active = useContext(AttentionActive);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [details, setDetails] = useState(false);
  const dialog = useRef<HTMLDialogElement>(null);
  const pending = useRef<{ kind: 'retry' | 'download'; id: string } | null>(null);
  const working = useRef(false);
  useEffect(() => { if (!active || value?.phase === 'ready') setDetails(false); }, [active, value?.phase]);
  useEffect(() => {
    if (!details) return;
    const element = dialog.current;
    element?.showModal();
    return () => element?.close();
  }, [details]);
  if (!active || value?.phase === 'ready') return null;
  const phase = value?.phase ?? 'not_configured';
  const progress = value?.download;
  async function run(kind: 'retry' | 'download' | 'install' | 'pause') {
    if (working.current) return;
    working.current = true;
    setBusy(true); setError('');
    try {
      let result: Status | null;
      if (kind === 'install') result = await window.intercat.installEditor();
      else if (kind === 'pause') result = await window.intercat.pauseEditorDownload();
      else {
        if (pending.current?.kind !== kind) pending.current = { kind, id: await window.intercat.createOperationId() };
        result = await (kind === 'retry' ? window.intercat.retryEditor(pending.current.id) : window.intercat.downloadEditor(pending.current.id));
      }
      pending.current = null;
      if (result) { update(result); setDetails(false); }
    } catch (failure) { setError(failure instanceof Error ? failure.message : 'The Editor action could not be completed.'); }
    finally { working.current = false; setBusy(false); }
  }
  const title = phase === 'starting' ? 'Editor starting · words wait'
    : phase === 'downloading' ? 'Downloading Editor · words wait'
    : phase === 'download_paused' ? 'Editor download paused · words wait'
    : 'Editor unavailable · words wait · Stop still works';
  return <section className="editor-status" aria-label="Local Editor">
    <div><strong role="status">{title}</strong>
      {phase === 'not_configured' && <p>Download Glimmer from Hugging Face (19.4 GB). It runs on this Mac. Choose Python from your MLX environment to begin.</p>}
      {phase === 'starting' && value && value.restart > 0 && <p>Automatic restart {value.restart} of 3</p>}
      {phase === 'downloading' && progress && <p>{progress.phase === 'checking' || progress.phase === 'verifying' ? 'Checking files' : 'Downloading'} · {(progress.received / 1e9).toFixed(1)} / {(progress.total / 1e9).toFixed(1)} GB</p>}
      {phase === 'downloading' && progress && <progress aria-label="Editor download" max={progress.total} value={progress.received} />}
      {error && <p role="alert">{error}</p>}
    </div>
    <div className="editor-actions" data-card-actions>
      {phase === 'not_configured' && <button disabled={busy} onClick={() => { void run('install'); }}>Download Glimmer</button>}
      {phase === 'downloading' && <button disabled={busy} onClick={() => { void run('pause'); }}>Pause download</button>}
      {phase === 'download_paused' && <button disabled={busy} onClick={() => { void run('download'); }}>Resume download</button>}
      {value?.canRetry && <button disabled={busy} onClick={() => { void run('retry'); }}>Retry</button>}
      {phase !== 'not_configured' && <button onClick={() => setDetails(true)}>Details</button>}
    </div>
    {details && createPortal(<dialog className="editor-details" ref={dialog} aria-label="Editor details" onCancel={event => { event.preventDefault(); setDetails(false); }} onKeyDown={event => { if (event.key === 'Escape') event.stopPropagation(); }}>
      <h2>Local Editor</h2><p>{value?.detail || 'The Editor is preparing its local model.'}</p>
      <p>Download again checks the pinned Glimmer files and replaces missing or damaged files in the app’s storage. Your original model folder is kept.</p>
      {error && <p role="alert">{error}</p>}
      <div className="editor-actions" data-card-actions><button autoFocus onClick={() => setDetails(false)}>Close</button><button disabled={busy || phase === 'downloading'} onClick={() => { void run('install'); }}>Choose Python environment</button><button disabled={busy || phase === 'downloading'} onClick={() => { void run('download'); }}>Download again</button></div>
    </dialog>, document.body)}
  </section>;
}
