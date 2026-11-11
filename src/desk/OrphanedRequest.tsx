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
  const isReply = !!word.replyTo || !!word.planReply;
  const disabled = !!word.editHold;
  return <section className="desk-card orphaned-request" role="group" aria-label={isReply ? 'Reply needs a destination' : 'Instruction needs a destination'}>
    <div>
      <small className="request-state">Not started · pick a track</small>
      <strong><PrivacyText text={word.reason || (isReply ? 'This reply’s question is no longer current' : 'Choose where to send this instruction')} interactive={false} /></strong>
      {isReply && <details><summary>Original question</summary><p><PrivacyText text={word.planReply?.question ?? question?.text ?? 'The original question is unavailable.'} /></p></details>}
      <p className="orphaned-reply" tabIndex={0}><PrivacyText text={word.text} /></p>
      <div className="request-options" data-card-actions>
        <button disabled={disabled} onClick={edit}>Edit</button>
        <label>Send to<select aria-label={isReply ? 'Reply destination' : 'Instruction destination'} value={destination} disabled={disabled} onChange={event => setDestination(event.target.value)}>
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
