import { useEffect, useRef, useState } from 'react';
import type { PrivacyChange, PrivacyState } from '../bridge';

export function PrivacySettings({ value, enabled, localReady, saved }: { value: PrivacyState; enabled: boolean; localReady: boolean; saved: (value: PrivacyState) => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<PrivacyChange | null>(null);
  useEffect(() => { pending.current = null; setError(''); }, [value.revision]);
  async function change(mode?: PrivacyState['mode']) {
    if (busy) return;
    setBusy(true); setError('');
    try {
      if (mode) pending.current = { opId: await window.intercat.createOperationId(), revision: value.revision, mode };
      if (!pending.current) return;
      saved(await window.intercat.changePrivacy(pending.current));
      pending.current = null;
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'The privacy setting could not be saved.'); }
    finally { setBusy(false); }
  }
  return <section className="privacy-setting" aria-label="Privacy gate">
    <label htmlFor="privacy-mode">Editor · Privacy gate</label>
    <select id="privacy-mode" value={value.mode} disabled={!enabled || busy} onChange={event => { void change(event.target.value as PrivacyState['mode']); }}>
      <option value="basic">Basic</option><option value="full" disabled={!localReady}>Full · local Editor</option>
    </select>
    <p>{value.mode === 'full' ? 'Also checks names, addresses and passwords on this Mac.' : 'Checks keys, tokens, email and phone numbers on this Mac.'}</p>
    <small>Voice audio, attached files and files read by workers can leave this Mac without this check. Worker search queries are not inspected.</small>
    {!localReady && <small>{value.mode === 'full' ? 'Full requests wait until the local Editor is ready. Stop and approval controls remain available.' : 'Start the local Editor to enable Full.'}</small>}
    {error && <p role="alert">{error}<button disabled={busy || !enabled} onClick={() => { void change(); }}>Retry privacy setting</button></p>}
  </section>;
}
