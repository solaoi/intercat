import { useState } from 'react';
import type { EditorAwayPage, EditorReturn } from '../bridge';
import { PrivacyText } from './PrivacyText';

export function AwayDetails({ interval, page }: { interval: EditorReturn; page: EditorAwayPage }) {
  const [entries, setEntries] = useState(page.entries);
  const [before, setBefore] = useState(page.before);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  async function earlier() {
    if (busy || before === null) return;
    setBusy(true); setError('');
    try {
      const next = await window.intercat.getEditorAway({ from: interval.from, to: interval.to, before });
      setEntries(current => [...current, ...next.entries.filter(entry => !current.some(prior => prior.id === entry.id))]);
      setBefore(next.before);
    } catch { setError('Earlier explanations could not be loaded. Try again.'); }
    finally { setBusy(false); }
  }
  return <div className="away-details">
    <p>{page.clips} closed {page.clips === 1 ? 'clip' : 'clips'}{page.effects ? ` · ${page.effects} ${page.effects === 1 ? 'effect' : 'effects'} outside Git` : ''}</p>
    {page.waiting.length > 0 && <p><PrivacyText text={`${page.waiting.join(', ')} ${page.waiting.length === 1 ? 'waits' : 'wait'} for you.`} /></p>}
    {entries.length === 0 && <p>No Editor explanations during this interval.</p>}
    <ol>{entries.map(entry => <li key={entry.id}>
      <time dateTime={new Date(entry.createdAt).toISOString()}>{new Date(entry.createdAt).toLocaleTimeString()}</time>
      <p><PrivacyText text={entry.line} /></p><p><PrivacyText text={entry.detail} /></p>
    </li>)}</ol>
    {error && <p role="alert">{error}</p>}
    {before !== null && <button disabled={busy} onClick={() => { void earlier(); }}>{busy ? 'Loading earlier…' : 'Earlier explanations'}</button>}
  </div>;
}
