import { useEffect, useRef, useState } from 'react';
import type { Stops, ProjectSecretState, LiveTrack } from '../bridge';
import { ProjectSecrets } from './ProjectSecrets';

export function PolicySettings({ open, stops, close, saved, project, tracks }: { open: boolean; stops: Stops; project?: ProjectSecretState | null; tracks: LiveTrack[]; close: () => void; saved: (stops: Stops) => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [choice, setChoice] = useState(stops);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [quit, setQuit] = useState<'ask'|'background'|'stop'>('ask');
  useEffect(() => {
    if (open) { setChoice(stops); setError(''); void window.intercat.getQuitPreference().then(setQuit).catch(()=>setError('Quit preference could not be loaded.')); dialog.current?.showModal(); }
    else dialog.current?.close();
  }, [open, stops]);
  async function save() {
    setSaving(true); setError('');
    try { const result = await window.intercat.setStops({ opId: await window.intercat.createOperationId(), stops: choice }); await window.intercat.setQuitPreference(quit); saved(result.stops); close(); }
    catch { setError('The setting could not be saved. Try again.'); }
    finally { setSaving(false); }
  }
  return <dialog ref={dialog} className="policy-settings" aria-labelledby="policy-title" onCancel={event => { event.preventDefault(); if (!saving) close(); }}>
    <header><h2 id="policy-title">Settings · App</h2><button disabled={saving} onClick={close} aria-label="Close settings">×</button></header>
    <label htmlFor="stops">Stops</label>
    <select id="stops" value={choice} disabled={saving} onChange={event => setChoice(event.target.value as Stops)}>
      <option value="irreversible-only">Irreversible only</option>
      <option value="every-approval">Every approval</option>
    </select>
    <p>{choice === 'irreversible-only' ? 'Stops the irreversible actions it recognizes.' : 'Asks you about every approval request from a worker.'}</p>
    <small>Applies to the next approval request. Some tools do not request approval.</small>
    <label htmlFor="quit-choice">On quit</label>
    <select id="quit-choice" value={quit} disabled={saving} onChange={event=>setQuit(event.target.value as typeof quit)}>
      <option value="ask">Ask every time</option><option value="background">Keep working in the background</option><option value="stop">Stop everything</option>
    </select>
    {open && project && <ProjectSecrets project={project} tracks={tracks} />}
    {error && <p role="alert">{error}</p>}
    <footer><button disabled={saving} onClick={close}>Cancel</button><button disabled={saving} onClick={() => { void save(); }}>{saving ? 'Saving…' : 'Save'}</button></footer>
  </dialog>;
}
