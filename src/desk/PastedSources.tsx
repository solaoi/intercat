import { useEffect, useState, type ClipboardEvent, type KeyboardEvent } from 'react';
import type { PasteDraft, useDrafts } from './drafts';
import { RefreshReference } from './ReferenceScreening';
import { OriginalPastedText } from './OriginalPastedText';

type Drafts = ReturnType<typeof useDrafts>;

function PastedBlock({ source, drafts, draftId, index, disabled, readOnly, paste, submit }: {
  source: PasteDraft;
  drafts: Drafts;
  draftId: string;
  index: number;
  disabled: boolean;
  readOnly: boolean;
  paste: (event: ClipboardEvent<HTMLTextAreaElement>, segment: string) => void;
  submit: (event: KeyboardEvent<HTMLTextAreaElement>) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  async function finish() {
    if (busy) return;
    setBusy(true); setError('');
    try {
      await drafts.flush();
      const attachment = await window.intercat.finishPastedText(source.id);
      drafts.completePaste(source.id, attachment, draftId);
    } catch { setError('The pasted text could not be saved. Retry keeps the same original.'); }
    finally { setBusy(false); }
  }
  useEffect(() => { if (!source.attachment) void finish(); }, [source.id]);
  return <>
    <section className="pasted-source" aria-label={`Pasted text ${index + 1}`}>
      <header><strong>Pasted text{source.words > 0 && <small> · {source.words.toLocaleString()} {source.words === 1 ? 'word' : 'words'}</small>}</strong>
        <button disabled={disabled || readOnly} aria-label={`Remove pasted text ${index + 1}`} onClick={() => drafts.removePaste(source.id)}>×</button>
      </header>
      <div className="paste-choices" role="group" aria-label={`Use pasted text ${index + 1} as`}>
        <button aria-pressed={source.role === 'request'} disabled={disabled || readOnly} onClick={() => drafts.setPaste(source.id, { role: 'request' })}>Keep as the request</button>
        <button aria-pressed={source.role === 'reference'} disabled={disabled || readOnly} onClick={() => drafts.setPaste(source.id, { role: 'reference' })}>Attach as reference</button>
      </div>
      {!source.attachment ? <div role="status">{error || 'Saving pasted text…'}{error && <button disabled={busy} onClick={() => { void finish(); }}>Retry pasted text</button>}</div> :
        <OriginalPastedText id={source.id} />}
      {source.attachment && <RefreshReference id={source.id} label={`pasted text ${index + 1}`} disabled={disabled || readOnly} />}
    </section>
    <textarea aria-label={`Text after pasted block ${index + 1}`} placeholder="Continue your request…" rows={2} value={source.after} disabled={disabled || readOnly}
      onChange={event => drafts.setPaste(source.id, { after: event.target.value })}
      onPaste={event => paste(event, source.id)} onKeyDown={submit} />
  </>;
}

export function PastedSources({ drafts, disabled, readOnly, paste, submit }: {
  drafts: Drafts;
  disabled: boolean;
  readOnly: boolean;
  paste: (event: ClipboardEvent<HTMLTextAreaElement>, segment: string) => void;
  submit: (event: KeyboardEvent<HTMLTextAreaElement>) => void;
}) {
  return <>{drafts.entry.sources?.map((source, index) => <PastedBlock key={`${drafts.session}/${drafts.book.active}/${source.id}`} source={source} index={index} drafts={drafts} draftId={drafts.book.active} disabled={disabled} readOnly={readOnly} paste={paste} submit={submit} />)}</>;
}
