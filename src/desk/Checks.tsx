import { useEffect, useRef, useState } from 'react';
import type { ChecksChange, ChecksState, CheckCommand } from '../bridge';

export function Checks() {
  const [state, setState] = useState<ChecksState | null>(null);
  const [folder, setFolder] = useState('');
  const [mode, setMode] = useState<CheckCommand['mode']>('auto');
  const [command, setCommand] = useState('');
  const [history, setHistory] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<ChecksChange | null>(null);
  const generation = useRef(0);
  function select(next: ChecksState, id: string) {
    const choice = next.setting.folders[id];
    setFolder(id); setMode(choice?.command.mode ?? 'auto');
    setCommand(choice?.command.mode === 'run' ? choice.command.command : '');
    setHistory(choice?.history ?? false);
  }
  async function load() {
    const request = ++generation.current;
    setBusy(true); setError('');
    try {
      const next = await window.intercat.getChecks();
      if (generation.current !== request) return;
      pending.current = null; setState(next);
      select(next, next.folders.some(value => value.id === folder) ? folder : next.folders[0]?.id ?? '');
    } catch { if (generation.current === request) setError('Check settings could not be loaded.'); }
    finally { if (generation.current === request) setBusy(false); }
  }
  useEffect(() => { void load(); return () => { generation.current += 1; }; }, []);
  const valid = mode !== 'run' || !!command.trim() && !/[\r\n\0]/.test(command) && new TextEncoder().encode(command).length <= 16384;
  async function save() {
    if (!state || !folder || busy || !valid) return;
    const requestGeneration = generation.current;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), revision: state.setting.revision, folder, command: mode === 'run' ? { mode, command } : { mode }, history };
      const setting = await window.intercat.changeChecks(pending.current);
      if (generation.current !== requestGeneration) return;
      pending.current = null; const next = { ...state, setting }; setState(next); select(next, folder);
    } catch (failure) {
      if (generation.current === requestGeneration) setError(failure instanceof Error ? failure.message : 'Check settings could not be saved.');
    } finally { if (generation.current === requestGeneration) setBusy(false); }
  }
  const saved = state?.setting.folders[folder];
  const changed = mode !== (saved?.command.mode ?? 'auto') || history !== (saved?.history ?? false) || mode === 'run' && command !== (saved?.command.mode === 'run' ? saved.command.command : '');
  const disabled = busy || !folder || !!pending.current;
  return <section className="checks-setting" aria-label="Checks">
    <h3>Agents · Checks</h3>
    {state && state.folders.length > 0 ? <>
      <label htmlFor="check-folder">Folder</label>
      <select id="check-folder" value={folder} disabled={busy || !!pending.current} onChange={event => select(state, event.target.value)}>{state.folders.map(value => <option key={value.id} value={value.id}>{value.name}</option>)}</select>
      <label htmlFor="check-command-mode">Check command</label>
      <select id="check-command-mode" value={mode} disabled={disabled} onChange={event => setMode(event.target.value as CheckCommand['mode'])}><option value="auto">Detect automatically</option><option value="run">Custom command</option><option value="disabled">No check</option></select>
      {mode === 'run' && <><label htmlFor="check-command">Command</label><input id="check-command" value={command} disabled={disabled} spellCheck={false} onChange={event => setCommand(event.target.value)} /></>}
      <label className="check-history" htmlFor="check-history"><input id="check-history" aria-describedby="check-history-disclosure" type="checkbox" checked={history} disabled={disabled} onChange={event => setHistory(event.target.checked)} />Checks need Git history</label>
      <p id="check-history-disclosure">When enabled, Check and project hooks can read full Git history and tags, including removed files and secrets.</p>
      <small>Applies to the next Commit review. Re-check keeps the current candidate and uses the latest settings.</small>
      <div className="checks-setting-actions"><button disabled={busy || !valid || !changed && !pending.current} onClick={() => { void save(); }}>{busy ? 'Saving…' : pending.current ? 'Retry Check settings' : 'Apply Check settings'}</button></div>
    </> : state ? <p>Choose a project to configure Checks.</p> : !error && <p role="status">Loading Check settings…</p>}
    {error && <div role="alert"><p>{error}</p><button disabled={busy} onClick={() => { void load(); }}>Reload Check settings</button></div>}
  </section>;
}
