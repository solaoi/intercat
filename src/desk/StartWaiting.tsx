import { GeneratedRequestTitle } from './RequestTitle';
import { PrivacyText } from './PrivacyText';
import type { InputResolution, Utterance } from '../bridge';

export function StartWaiting({ word, resolve, edit, cut }: {
  word: Utterance;
  resolve: (resolution: InputResolution) => void;
  edit: () => void;
  cut: () => void;
}) {
  return <div className="desk-card request-question" role="group" aria-label="Start waiting work">
    <span className="card-mark">◇</span>
    <div><GeneratedRequestTitle word={word} /><strong><PrivacyText text={word.reason ?? ''} interactive={false} /></strong>
      <small className="request-state">Only requests waiting for your go can start here.</small>
      <ul className="waiting-requests">
        {word.startWaiting?.candidates.map(candidate => <li key={candidate.inputId}>
          <button disabled={!candidate.available} onClick={() => resolve({ action: 'startWaiting', choice: { inputId: candidate.inputId, revision: candidate.revision } })}>
            <strong>{candidate.label ? `Start Worker ${candidate.label}` : 'Start request'}</strong>
            <span><PrivacyText text={candidate.text} interactive={false} /></span>
            {!candidate.available && <small>Not available</small>}
          </button>
        </li>)}
      </ul>
      <div className="request-options" data-card-actions>
        <button onClick={() => resolve({ action: 'retry', target: word.target })}>Read again</button>
        <button onClick={() => resolve({ action: 'hold' })}>Hold</button>
        <button onClick={edit}>Edit request</button>
        <button onClick={cut}>Take back</button>
      </div>
    </div>
  </div>;
}
