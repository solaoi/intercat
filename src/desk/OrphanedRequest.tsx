import { PrivacyText, usePrivacyLabels } from './PrivacyText';
import { useState } from 'react';
import type { InputResolution, LiveTrack, Utterance } from '../bridge';

export function OrphanedRequest({ word, tracks, edit, resolve, takeBack }: {
  word: Utterance;
  tracks: LiveTrack[];
  edit: () => void;
  resolve: (resolution: InputResolution) => void;
  takeBack: () => void;
}) {
  const label = usePrivacyLabels(tracks.map(track => track.title));
  const [destination, setDestination] = useState('');
  const question = word.replyTo?.question;
  const disabled = !!word.editHold;
  return <section className="desk-card orphaned-request" role="group" aria-label="Reply needs a destination">
    <div>
      <small className="request-state">Not started · pick a track</small>
      <strong><PrivacyText text={word.reason || 'This reply’s question is no longer current'} interactive={false} /></strong>
      <details><summary>Original question</summary><p><PrivacyText text={question?.text ?? 'The original question is unavailable.'} /></p></details>
      <p className="orphaned-reply" tabIndex={0}><PrivacyText text={word.text} /></p>
      <div className="request-options" data-card-actions>
        <button disabled={disabled} onClick={edit}>Edit</button>
        <label>Send to<select aria-label="Reply destination" value={destination} disabled={disabled} onChange={event => setDestination(event.target.value)}>
          <option value="">Choose a destination</option>
          {tracks.map(track => <option key={track.id} value={track.id}>Worker {track.label} · {label(track.title)}</option>)}
          <option value="new">New track</option>
        </select></label>
        <button disabled={disabled || !destination} onClick={() => resolve({ action: 'retry', target: destination === 'new' ? { mode: 'new' } : { mode: 'lock', trackId: destination } })}>Send instruction</button>
        <button disabled={disabled} onClick={takeBack}>Take back</button>
      </div>
    </div>
  </section>;
}
