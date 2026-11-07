import { GeneratedRequestTitle } from './RequestTitle';
import { PrivacyText, usePrivacyLabels } from './PrivacyText';
import type { useDrafts } from './drafts';
import { RephraseRequest } from './RephraseRequest';
import type { InputChoice, InputResolution, LiveTrack, Utterance } from '../bridge';

const commands = [
  ['commit', 'Commit'], ['push', 'Push'], ['open-pr', 'Open pull request'], ['export', 'Export'],
  ['branch-from', 'Branch from…'], ['restore', 'Restore…'], ['stage-mode', 'Stage mode'],
  ['settings', 'Settings'], ['plan-only', 'Plan only'], ['start-waiting', 'Start waiting work'],
];

export function RequestQuestion({ question, tracks, resolve, edit, cut, drafts, updated }: {
  question: Utterance; tracks: LiveTrack[]; resolve: (resolution: InputResolution) => void;
  edit: () => void; cut: () => void; drafts: ReturnType<typeof useDrafts>; updated: (word: Utterance) => void;
}) {
  const label = usePrivacyLabels(tracks.map(track => track.title));
  if (question.rephrase) return <RephraseRequest word={question} drafts={drafts} updated={updated} hold={() => resolve({ action: 'hold' })} cut={cut} />;
  const stage = question.review?.stage;
  const track = tracks.find(track => track.id === (question.deadline?.trackId ?? question.decision?.trackId));
  const lockedDeadline = !!question.deadline?.trackId && !!track;
  const newDeadline = !!question.deadline && question.target.mode === 'new';
  const recommendation = question.deadline?.recommendation;
  const candidates = tracks.slice(0, 8);
  const recommended = tracks.find(track => track.id === recommendation?.trackId);
  const extraRecommended = recommended && !candidates.includes(recommended) ? recommended : null;
  const chooseTrack = !lockedDeadline && !newDeadline && (!stage || stage === 'lane' || (stage === 'operation' && !track));
  const confirm = (choice: InputChoice) => resolve({ action: 'confirm', choice });
  return <div className="desk-card request-question" role="group" aria-label="Request needs attention" data-deadline={!!question.deadline}>
    <span className="card-mark">◇</span>
    <div><GeneratedRequestTitle word={question} /><strong><PrivacyText text={question.reason ?? ''} interactive={false} /></strong><small className="request-state">{chooseTrack ? 'Not started · pick a track' : 'Not started'}</small><details className="request-context"><summary aria-label="Read the full request" ><PrivacyText text={question.text} interactive={false} /></summary><p tabIndex={0} aria-label="Full request"><PrivacyText text={question.text} /></p></details>
      {chooseTrack && <small>Picking a track re-reads what to do.</small>}
      {stage === 'start-turn' && <small>{question.startCondition?.candidates.length ? 'Choose the turn that was running when you sent this request.' : 'No running turn was recorded with this request. Choose when to start or edit the request.'}</small>}
      <div className="request-options" data-card-actions>
        {chooseTrack && <>{candidates.map(track => <button className="request-destination" key={track.id} title={label(track.title)} onClick={() => resolve({ action: 'retry', target: { mode: 'lock', trackId: track.id } })}>Worker {track.label}{recommendation?.trackId === track.id && <small className="request-recommended">recommended</small>}</button>)}
          <button className="request-destination" onClick={() => resolve({ action: 'retry', target: { mode: 'new' } })}>New track{recommendation && recommendation.trackId === null && <small className="request-recommended">recommended</small>}</button></>}
        {(lockedDeadline || stage === 'operation' || stage === 'stale-turn') && track && <button onClick={() => confirm({ kind: 'instruction' })}>Send to {track.label} as an instruction</button>}
        {newDeadline && <button onClick={() => confirm({ kind: 'instruction' })}>Start a new track now</button>}
        {stage === 'trim' && <><button onClick={() => confirm({ kind: 'stop-and-send' })}>Stop and send</button><button onClick={() => confirm({ kind: 'after-turn' })}>Send after this turn</button></>}
        {stage === 'boundary' && <><button onClick={() => confirm({ kind: 'now' })}>{question.review?.turn ? `Stop ${track?.label ?? ''} and send` : 'Now'}</button><button onClick={() => confirm({ kind: 'after-turn' })}>After this turn</button><button onClick={() => confirm({ kind: 'when-told' })}>When I say</button></>}
        {stage === 'start-turn' && <>
          {question.startCondition?.candidates.map(source => <button key={`${source.sessionId}/${source.turnId}`} onClick={() => confirm({ kind: 'after-worker-turn', source })}>After {source.label}’s turn</button>)}
          <button onClick={() => confirm({ kind: 'now' })}>Now</button>
          <button onClick={() => confirm({ kind: 'when-told' })}>When I say</button>
        </>}
        {stage === 'command' && commands.map(([command, label]) => <button key={command} onClick={() => confirm({ kind: 'command', command })}>{label}</button>)}
        <button onClick={() => resolve({ action: 'hold' })}>Hold</button>
        <button onClick={edit}>Edit request</button><button onClick={cut}>Take back</button>
        {chooseTrack && question.deadline && tracks.length > 8 && <span className="request-recommendation-slot">{extraRecommended && <button className="request-destination" title={label(extraRecommended.title)} onClick={() => resolve({ action: 'retry', target: { mode: 'lock', trackId: extraRecommended.id } })}>Worker {extraRecommended.label}<small className="request-recommended">recommended</small></button>}</span>}
      </div>
    </div>
  </div>;
}
