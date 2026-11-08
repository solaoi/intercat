import { waitSourceLabel } from '../waitSource';
import type { HostMemoryStatus, ConfirmationStatus, InputResolution, LiveTrack, RedirectState, SchedulingStatus, Utterance } from '../bridge';
import { ConfirmationChip } from './Confirmation';
import { queueLabel, waitingForMemory } from '../scheduling';
import { requestTitle } from './RequestTitle';
import type { useDrafts } from './drafts';
import { RequestQuestion } from './RequestQuestion';
import { OrphanedRequest } from './OrphanedRequest';
import { StartWaiting } from './StartWaiting';
import type { AttentionCard } from './NeedsYou';
import { PrivacyText, usePrivacyText } from './PrivacyText';

interface Props {
  scheduling?: SchedulingStatus;
  hostMemory?: HostMemoryStatus;
  words: Utterance[];
  redirects: RedirectState[];
  tracks: LiveTrack[];
  editingTokens: string[];
  confirmation?: ConfirmationStatus;
  onConfirmed: (word: Utterance) => void;
  onConfirmationChanged: (value: ConfirmationStatus) => void;
  onSelect: (word: Utterance) => void;
  onCut: (word: Utterance) => void;
  onStart: (word: Utterance) => void;
  onEdit: (word: Utterance) => void;
  onResolve: (word: Utterance, resolution: InputResolution) => void;
}

function resultLabel(word: Utterance, tracks: LiveTrack[], redirects: RedirectState[], editingTokens: string[], scheduling?: SchedulingStatus, hostMemory?: HostMemoryStatus) {
  if (word.editHold) return editingTokens.includes(word.editHold.token) ? 'editing' : 'editing in another window';
  if (word.dependencyRetryRunnable === false) return 'held · retry source changed';
  if (word.state === 'held' && word.reason === 'editing was interrupted') return 'held · editing was interrupted';
  if (word.state === 'reading') return 'checking';
  if (word.state === 'staged') return 'waiting for editor';
  if (word.reading?.kind === 'park' && ['queued', 'running'].includes(word.reading.state)) return 'checking';
  if (word.decision?.kind === 'park') return 'parked · say where';
  if (word.state === 'orphaned') return word.reason || 'needs a destination';
  const track = tracks.find(track => track.id === word.decision?.trackId);
  if (word.state === 'placed' && word.decision?.boundary !== 'when-told' && (!word.startCondition || word.startCondition.ready) && (waitingForMemory({ hostMemory }, word.decision?.trackId ?? undefined) || waitingForMemory({ hostMemory }, word.id))) return `to ${track?.label ?? '?'} · queued · waiting for memory`;
  if (word.reasonReview && ['sent', 'landed'].includes(word.state)) return `sent with ${word.reasonReview.label}’s reasons`;
  if (word.decision?.kind === 'trim') {
    const redirect = redirects.find(redirect => redirect.inputId === word.id);
    let state = 'waiting to send';
    if (word.state === 'held' || redirect?.phase === 'held') {
      state = 'held';
    } else if (word.state === 'sent' || redirect?.phase === 'sent' || redirect?.phase === 'done') {
      state = `sent · waiting for ${track?.label ?? 'the worker'} to pick it up`;
    } else if (redirect?.phase === 'interrupting') {
      state = 'sent · stopping';
    } else if (redirect?.phase === 'stopping') {
      state = 'waiting to stop';
    }
    return `redirect ${track?.label ?? '?'} · ${state}`;
  }
  if (word.startCondition?.detail && !word.startCondition.ready) return `held · ${word.startCondition.detail}`;
  if (word.state !== 'placed') return word.state;
  const queued = scheduling?.waiting.find(entry => entry.id === word.id);
  if (queued) return `to ${track?.label ?? '?'} · ${queueLabel(queued.position)}`;
  if (word.decision?.kind === 'command') return `${word.decision.command} requested`;
  const destination = word.decision?.kind === 'fork' ? 'new track' : word.decision?.kind === 'plan' ? 'planned' : `to ${track?.label ?? '?'}`;
  if (word.startCondition?.source && !word.startCondition.ready) return `${destination} · ${word.startCondition.detail ?? `after ${waitSourceLabel(word.startCondition.source)}`}`;
  return word.decision?.boundary === 'when-told' ? `${destination} · waiting for your go` : destination;
}

export function requestCards({ words, tracks, onResolve, onEdit, onCut, drafts, onRephrased, onReviewPlan }: Pick<Props, 'words' | 'tracks' | 'onResolve' | 'onEdit' | 'onCut'> & { drafts: ReturnType<typeof useDrafts>; onRephrased: (word: Utterance) => void; onReviewPlan: (id: string) => void }): AttentionCard[] {
  return words.filter(word => !word.confirmation && (word.state === 'asked' || word.state === 'orphaned')).map(word => {
    const track = tracks.find(track => track.id === (word.target.mode === 'lock' ? word.target.trackId : word.decision?.trackId));
    return {
      id: `request-${word.id}`,
      kind: 'question',
      version: word.revision,
      destination: track ? { trackId: track.id, label: track.label } : undefined,
      title: word.startWaiting && !word.review ? `Start waiting work · ${requestTitle(word)}` : word.state === 'asked' ? `Can’t place “${requestTitle(word)}”` : `Reply needs a destination · ${requestTitle(word)}`,
      content: word.state === 'orphaned'
        ? <OrphanedRequest word={word} tracks={tracks} edit={() => onEdit(word)} resolve={resolution => onResolve(word, resolution)} takeBack={() => onCut(word)} />
        : word.startWaiting && !word.review
          ? <StartWaiting word={word} reviewPlan={onReviewPlan} resolve={resolution => onResolve(word, resolution)} edit={() => onEdit(word)} cut={() => onCut(word)} />
          : <RequestQuestion drafts={drafts} updated={onRephrased} question={word} tracks={tracks} resolve={resolution => onResolve(word, resolution)} edit={() => onEdit(word)} cut={() => onCut(word)} />,
    };
  });
}

function RequestChip({ word, tracks, redirects, editingTokens, scheduling, hostMemory, onSelect, onCut, onStart }: Pick<Props, 'hostMemory' | 'scheduling' | 'tracks' | 'redirects' | 'editingTokens' | 'onSelect' | 'onCut' | 'onStart'> & { word: Utterance }) {
  const { view, error } = usePrivacyText(requestTitle(word));
  const label = view?.text ?? error ?? 'Checking privacy…';
  return <div className={`request-chip ${word.startCondition?.detail && !word.startCondition.ready ? 'held' : word.state}`}>
        <button className="request-select" data-utterance-id={word.id} aria-haspopup="dialog" title={label} onClick={() => onSelect(word)}>
          <span>{word.decision?.kind === 'park' ? '◇' : '◆'}</span><span className="request-text">{label}</span>
          {word.attachments.length > 0 && <small className="request-attachments" aria-label="Attached reference files" title={word.attachments.map(file => file.name).join(', ')}>📎 {word.attachments.map(file => file.name).join(', ')}</small>}
          <small className="request-result"><PrivacyText text={resultLabel(word, tracks, redirects, editingTokens, scheduling, hostMemory)} interactive={false} /></small>
        </button>
        {(word.state === 'held' || word.state === 'placed' && word.decision?.boundary === 'when-told') && !word.editHold && <button disabled={word.dependencyRetryRunnable === false || !!word.startCondition && !word.startCondition.ready} onClick={() => onStart(word)} aria-label={`Start request: ${label}`}>Start</button>}
        {!word.editHold && !word.interruptStarted && !['redirecting', 'starting', 'sent', 'landed', 'orphaned'].includes(word.state) && <button aria-label={`Cut request: ${label}`} onClick={event => { event.stopPropagation(); onCut(word); }} onKeyDown={event => {
          if (['Enter', 'Delete', 'Backspace'].includes(event.key)) { event.preventDefault(); event.stopPropagation(); onCut(word); }
        }}>×</button>}
      </div>;
}

export function Requests({ words, redirects, tracks, editingTokens, scheduling, hostMemory, onSelect, onCut, onStart, confirmation, onConfirmed, onConfirmationChanged }: Omit<Props, 'onEdit' | 'onResolve'>) {
  const visible = words.filter(word => !word.reviewFix && (!word.planItemId || word.planContinuation) && !['cut', 'landed'].includes(word.state) && (word.state !== 'asked' || !!word.confirmation));
  return <>
    <div className="draft-strip request-strip" data-empty={!visible.length}>
      <span className="draft-label">REQUESTS</span>
      {visible.length ? visible.map(word => word.confirmation
        ? <ConfirmationChip key={`${word.id}/${word.revision}/${word.confirmation.decisionId}`} word={word} tracks={tracks} mode={confirmation} edit={() => onSelect(word)} cut={() => onCut(word)} confirmed={onConfirmed} modeChanged={onConfirmationChanged} />
        : <RequestChip key={word.id} scheduling={scheduling} hostMemory={hostMemory} word={word} tracks={tracks} redirects={redirects} editingTokens={editingTokens} onSelect={onSelect} onCut={onCut} onStart={onStart} />) : <span>Nothing queued</span>}
    </div>
  </>;
}
