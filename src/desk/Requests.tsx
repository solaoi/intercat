import type { InputResolution, LiveTrack, RedirectState, Utterance } from '../bridge';

interface Props {
  words: Utterance[];
  redirects: RedirectState[];
  tracks: LiveTrack[];
  onSelect: (word: Utterance) => void;
  onCut: (word: Utterance) => void;
  onStart: (word: Utterance) => void;
  onEdit: (word: Utterance) => void;
  onResolve: (word: Utterance, resolution: InputResolution) => void;
}
export function Requests({ words, redirects, tracks, onSelect, onCut, onStart, onEdit, onResolve }: Props) {
  const visible = words.filter(word => word.state !== 'cut' && word.state !== 'landed');
  const question = visible.find(word => word.state === 'asked');
  return <>
    {question && <div className="desk-card request-question" role="group" aria-label="Request needs attention">
      <span className="card-mark">◇</span>
      <div><strong>{question.reason}</strong><p title={question.text}>{question.text}</p><small>Picking a track re-reads what to do.</small>
        <div className="request-options">{tracks.slice(0, 8).map(track => <button key={track.id} title={track.title} onClick={() => onResolve(question, { action: 'retry', target: { mode: 'lock', trackId: track.id } })}>Worker {track.label}</button>)}
          <button onClick={() => onResolve(question, { action: 'retry', target: { mode: 'new' } })}>New track</button>
          <button onClick={() => onResolve(question, { action: 'hold' })}>Hold</button>
          <button onClick={() => onEdit(question)}>Edit request</button><button onClick={() => onCut(question)}>Cut</button>
        </div>
      </div>
    </div>}
    <div className="draft-strip request-strip"><span className="draft-label">REQUESTS</span>{visible.length ? visible.map(word => <div key={word.id} className={`request-chip ${word.state}`}>
      <button className="request-select" title={word.text} onClick={() => onSelect(word)}><span>◆</span><span className="request-text">{word.text}</span>{word.attachments.length > 0 && <small className="request-attachments" aria-label="Attached reference files" title={word.attachments.map(file => file.name).join(', ')}>📎 {word.attachments.map(file => file.name).join(', ')}</small>}<small>{word.decision?.kind === 'trim' ? `redirect ${tracks.find(track => track.id === word.decision?.trackId)?.label ?? '?'} · ${redirects.find(redirect => redirect.inputId === word.id)?.phase === 'stopping' ? 'stopping' : word.state === 'held' ? 'held' : 'waiting for worker'}` : word.state === 'reading' ? 'checking' : word.state === 'staged' ? 'waiting for editor' : word.decision?.kind === 'command' ? `${word.decision.command} requested` : word.state === 'placed' ? word.decision?.kind : word.state}</small></button>
      {word.state === 'held' && <button onClick={() => onStart(word)} aria-label={`Start request: ${word.text}`}>Start</button>}
      {!['redirecting', 'starting', 'sent', 'landed', 'orphaned'].includes(word.state) && <button aria-label={`Cut request: ${word.text}`} onClick={event => { event.stopPropagation(); onCut(word); }} onKeyDown={event => { if (['Enter', 'Delete', 'Backspace'].includes(event.key)) { event.preventDefault(); event.stopPropagation(); onCut(word); } }}>×</button>}
    </div>) : <span>Nothing queued</span>}</div>
  </>;
}
