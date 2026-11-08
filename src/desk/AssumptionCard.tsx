import { useRef, useState } from 'react';
import type { Assumption, AssumptionCommand, LiveTrack } from '../bridge';
import type { AttentionCard } from './NeedsYou';

function AssumptionCard({ assumption, label }: { assumption: Assumption; label: string }) {
  const [choice, setChoice] = useState(assumption.options.find(option => option !== assumption.text) ?? '');
  const [changing, setChanging] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<AssumptionCommand | null>(null);

  async function answer(value: AssumptionCommand['answer']) {
    setBusy(true);
    setError('');
    try {
      const command = pending.current ?? {
        opId: await window.intercat.createOperationId(),
        assumptionId: assumption.id,
        revision: assumption.revision,
        answer: value,
      };
      pending.current = command;
      await window.intercat.answerAssumption(command);
      pending.current = null;
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : 'The answer could not be saved.');
    } finally {
      setBusy(false);
    }
  }

  return <details className="assumption-card">
    <summary>{assumption.source === 'planning' ? 'Plan assumed' : 'Worker'} {label} · {assumption.text}</summary>
    <p>{assumption.questionId && 'Worker asked · '}{assumption.question}</p>
    <blockquote>{assumption.evidence}</blockquote>
    <p className="assumption-note">Settled at commit unless you say otherwise.</p>
    {changing && <label>Use instead
      <textarea aria-label="Change assumption" value={choice}
        disabled={busy || !!pending.current} onChange={event => setChoice(event.target.value)} />
    </label>}
    {error && <p role="alert">{error}</p>}
    <div className="assumption-actions" data-card-actions>
      {pending.current ? <button disabled={busy} onClick={() => { void answer(pending.current!.answer); }}>Retry saving answer</button> : <>
        <button disabled={busy} onClick={() => { void answer({ action: 'keep' }); }}>Keep</button>
        {changing ? <>
          <button disabled={busy} onClick={() => setChanging(false)}>Cancel</button>
          <button disabled={busy || !choice.trim()} onClick={() => { void answer({ action: 'change', text: choice }); }}>Send correction</button>
        </> : <button disabled={busy} onClick={() => setChanging(true)}>Change</button>}
      </>}
    </div>
  </details>;
}

export function assumptionCards(assumptions: Assumption[], selected: string | null, tracks: LiveTrack[]): AttentionCard[] {
  return assumptions.filter(assumption => assumption.state === 'open' && assumption.trackId === selected)
    .map(assumption => ({
      id: `assumption-${assumption.id}`,
      kind: 'assumption',
      destination: { trackId: assumption.trackId, label: tracks.find(track => track.id === assumption.trackId)?.label ?? '' },
      version: assumption.revision,
      title: assumption.text,
      content: <AssumptionCard key={`${assumption.id}:${assumption.revision}`} assumption={assumption}
        label={tracks.find(track => track.id === assumption.trackId)?.label ?? ''} />,
    }));
}
