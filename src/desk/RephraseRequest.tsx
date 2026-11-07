import { useEffect, useState } from 'react';
import type { Utterance } from '../bridge';
import type { useDrafts } from './drafts';
import { PrivacyText } from './PrivacyText';
import { OriginalPastedText } from './OriginalPastedText';

export function RephraseRequest({ word, drafts, updated, hold, cut }: {
  word: Utterance; drafts: ReturnType<typeof useDrafts>; updated: (word: Utterance) => void;
  hold: () => void; cut: () => void;
}) {
  const id = `rephrase:${word.id}`;
  const entry = drafts.book.entries[id];
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => { drafts.ensureRephrase(word); }, [drafts, drafts.ready, word.id, word.revision, word.rephrase?.proposal]);
  async function submit() {
    if (busy || !entry?.text.trim()) return;
    if (!entry.pending && /[\p{Cc}]/u.test(entry.text)) { setError('Write the instruction on a single line.'); return; }
    setBusy(true); setError('');
    try {
      const pending = await drafts.prepare({ kind: 'rephrase', request: {
        opId: await window.intercat.createOperationId(), utteranceId: word.id,
        revision: word.revision, text: entry.text,
      } }, id);
      if (pending.kind !== 'rephrase') throw new Error('Another draft operation is pending.');
      updated(await window.intercat.rephraseUtterance(pending.request));
      drafts.remove(id);
    } catch (cause) { setError(cause instanceof Error ? cause.message : 'The instruction could not be saved.'); }
    finally { setBusy(false); }
  }
  return <form className="desk-card request-question rephrase-request" aria-label="Shorten request" onSubmit={event => { event.preventDefault(); void submit(); }}>
    <span className="card-mark">◇</span><div>
      <strong><PrivacyText text={word.reason ?? ''} interactive={false} /></strong><small className="request-state">Not started · review the instruction before sending</small>
      <details className="request-context"><summary>Original request</summary><p tabIndex={0}><PrivacyText text={word.sourceText ?? word.text} /></p>{word.sources?.map((source, index) => <div key={source.attachmentId}><OriginalPastedText id={source.attachmentId} label={`Read original pasted text ${index + 1}`} /><p><PrivacyText text={source.after} interactive={false} /></p></div>)}</details>
      <label htmlFor={`rephrase-${word.id}`}>One-line request</label>
      <input id={`rephrase-${word.id}`} value={entry?.text ?? ''} disabled={!drafts.ready || busy || !!entry?.pending} maxLength={32 * 1024}
        onChange={event => { if (new TextEncoder().encode(event.target.value).length > 32 * 1024) { setError('This instruction is too long. Shorten it before sending.'); return; } setError(''); drafts.setRephraseText(id, event.target.value); }}
        onCompositionStart={() => drafts.setRephraseText(id, entry?.text ?? '')}
        onKeyDown={event => { if (event.nativeEvent.isComposing && event.key === 'Enter') event.preventDefault(); }} />
      <small role="status">{word.rephrase?.state === 'pending' || word.rephrase?.state === 'running' ? 'The Editor is drafting a suggestion. You can write your own now.' : word.rephrase?.state === 'failed' ? 'The Editor could not draft this request. Write a one-line instruction.' : word.rephrase?.state === 'complete' ? 'Editor draft ready. Review it before sending. The original text stays attached.' : 'The original text stays attached as references.'}</small>
      {word.rephrase?.proposal && entry?.text !== word.rephrase.proposal && <button type="button" disabled={busy || !!entry?.pending} onClick={() => drafts.setRephraseText(id, word.rephrase!.proposal!)}>Use Editor’s draft</button>}
      {(error || drafts.issue) && <p role="alert">{error || drafts.issue}</p>}
      <div className="request-options" data-card-actions>
        <button type="button" disabled={busy} onClick={hold}>Hold</button><button type="button" disabled={busy} onClick={cut}>Take back</button>
        <button type="submit" disabled={!drafts.ready || busy || !entry?.text.trim()}>{busy ? 'Saving…' : entry?.pending ? 'Retry send' : 'Send instruction'}</button>
      </div>
    </div>
  </form>;
}
