import { useEffect, useRef, useState } from 'react';
import type { CheckEnvironmentState, CheckEnvironmentChange } from '../bridge';

export function CheckEnvironment() {
  const [state, setState] = useState<CheckEnvironmentState | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<CheckEnvironmentChange | null>(null);
  useEffect(() => {
    let closed = false;
    void window.intercat.getCheckEnvironment().then(value => { if (!closed) setState(value); })
      .catch(() => { if (!closed) setError('Check environment could not be loaded.'); });
    return () => { closed = true; };
  }, []);

  async function apply(file: string | null, retry = false) {
    if (!state || busy) return;
    setBusy(true); setError('');
    try {
      const request = retry ? pending.current! : { opId: await window.intercat.createOperationId(), revision: state.selection.revision, file };
      pending.current = request;
      const selection = await window.intercat.changeCheckEnvironment(request);
      pending.current = null;
      setState({ ...state, selection });
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : 'Check environment could not be saved.');
    } finally { setBusy(false); }
  }

  async function choose() {
    if (busy) return;
    setBusy(true); setError('');
    let file: string | null = null;
    try { file = await window.intercat.chooseCheckEnvironment(); }
    catch { setError('The file chooser could not be opened.'); }
    finally { setBusy(false); }
    if (file !== null) void apply(file);
  }

  return <section className="check-environment" aria-label="Check environment">
    <h3>Agents · Check environment</h3>
    <p>Pass values from an untracked dotenv file to Check. The file stays outside its workspace.</p>
    {state ? <>
      <p className="check-environment-file">{state.selection.file ?? 'None'}</p>
      <div className="check-environment-actions">
        <button disabled={busy || !state.available || !!pending.current} onClick={() => { void choose(); }}>Choose file…</button>
        <button disabled={busy || !state.selection.file || !!pending.current} onClick={() => { void apply(null); }}>Use no file</button>
        {state.candidates.filter(file => file !== state.selection.file).map(file => <button key={file} disabled={busy || !!pending.current} title={file} onClick={() => { void apply(file); }}>Use {file.split('/').at(-1)}</button>)}
      </div>
      <small>{state.available ? 'Applies to the next Commit review. Use Re-check to update an open review.' : 'Choose a project to configure Check environment.'}</small>
    </> : !error && <p role="status">Loading Check environment…</p>}
    {error && <div role="alert"><p>{error}</p>{pending.current && <button disabled={busy} onClick={() => { void apply(pending.current!.file, true); }}>Retry environment save</button>}<button disabled={busy} onClick={() => { pending.current = null; setError(''); void window.intercat.getCheckEnvironment().then(setState).catch(() => setError('Check environment could not be loaded.')); }}>Reload environment</button></div>}
  </section>;
}
