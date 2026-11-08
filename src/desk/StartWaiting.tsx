import { GeneratedRequestTitle } from './RequestTitle';
import { PrivacyText } from './PrivacyText';
import type { InputResolution, Utterance } from '../bridge';

export function StartWaiting({ word, resolve, reviewPlan, edit, cut }: {
  word: Utterance;
  resolve: (resolution: InputResolution) => void;
  reviewPlan: (id: string) => void;
  edit: () => void;
  cut: () => void;
}) {
  return <div className="desk-card request-question" role="group" aria-label="Start waiting work">
    <span className="card-mark">◇</span>
    <div><GeneratedRequestTitle word={word} /><strong><PrivacyText text={word.reason ?? ''} interactive={false} /></strong>
      <small className="request-state">Only work waiting for your go can start here.</small>
      <ul className="waiting-requests">
        {word.startWaiting?.candidates.map(candidate => <li key={candidate.kind === 'plan' ? `plan:${candidate.planId}` : `input:${candidate.inputId}`}>
          <button disabled={!candidate.available} onClick={() => resolve({ action: 'startWaiting', choice: candidate.kind === 'plan'
            ? { kind: 'plan', planId: candidate.planId, revision: candidate.revision }
            : { kind: 'input', inputId: candidate.inputId, revision: candidate.revision } })}>
            <strong>{candidate.kind === 'plan' ? 'Start plan' : candidate.label ? `Start Worker ${candidate.label}` : 'Start request'}</strong>
            <span><PrivacyText text={candidate.text} interactive={false} /></span>
          </button>
          {!candidate.available && <small><PrivacyText text={candidate.reason ?? 'Not available'} interactive={false} /></small>}
          {candidate.kind === 'plan' && <button onClick={() => reviewPlan(candidate.planId)}>Review plan</button>}
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
