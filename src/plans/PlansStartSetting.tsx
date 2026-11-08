import { useEffect, useRef, useState } from 'react';
import type { PlansStartChange, PlansStartSetting as Setting } from './types';

export function PlansStartSetting() {
  const [setting, setSetting] = useState<Setting | null>(null);
  const [choice, choose] = useState<Setting['mode']>('manual');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<PlansStartChange | null>(null);
  const live = useRef(true);
  async function load() {
    setBusy(true); setError('');
    try { const next = await window.intercat.getPlansStart(); if (live.current) { pending.current = null; setSetting(next); choose(next.mode); } }
    catch { if (live.current) setError('Plans start could not be loaded.'); }
    finally { if (live.current) setBusy(false); }
  }
  useEffect(() => { live.current = true; void load(); return () => { live.current = false; }; }, []);
  async function save() {
    if (!setting || busy) return;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), revision: setting.revision, mode: choice };
      const next = await window.intercat.changePlansStart(pending.current);
      if (live.current) { pending.current = null; setSetting(next); choose(next.mode); }
    } catch (failure) { if (live.current) setError(failure instanceof Error ? failure.message : 'Plans start could not be saved.'); }
    finally { if (live.current) setBusy(false); }
  }
  return <section className="execution-limits" aria-label="Plans start">
    <label htmlFor="plans-start">Editor · Plans start</label>
    <select id="plans-start" value={choice} disabled={busy || !setting || !!pending.current} onChange={event => choose(event.target.value as Setting['mode'])}>
      <option value="manual">Only when I press Start</option><option value="after-delay">Automatically after 5 s</option>
    </select>
    <p>Applies to the next plan. Automatic starts count only while the plan is visible and you are not interacting. Plan only always waits for your go.</p>
    {setting && <div className="execution-limits-actions"><button disabled={busy || !pending.current && choice === setting.mode} onClick={() => { void save(); }}>{busy ? 'Saving…' : pending.current ? 'Retry Plans start' : 'Apply Plans start'}</button></div>}
    {!setting && !error && <p role="status">Loading Plans start…</p>}
    {error && <div role="alert"><p>{error}</p><button disabled={busy} onClick={() => { void load(); }}>Reload Plans start</button></div>}
  </section>;
}
