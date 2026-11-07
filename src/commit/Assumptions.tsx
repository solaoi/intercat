import { useState } from 'react';
import type { CommitAssumption, IntegrationJob } from './types';

function Row({ assumption, label, disabled, keep, change }: {
  assumption: CommitAssumption; label: string; disabled: boolean;
  keep: () => void; change: (text: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [text, setText] = useState(assumption.options.find(value => value !== assumption.text) ?? '');
  return <article>
    <strong>Worker {label} · {assumption.text}</strong>
    <p>{assumption.questionId && 'Worker asked · '}{assumption.question}</p>
    <blockquote>{assumption.evidence}</blockquote>
    {assumption.state === 'open' ? <>
      {editing && <label>Use instead<textarea aria-label={`Change Worker ${label} assumption`} value={text}
        disabled={disabled} onChange={event => setText(event.target.value)} /></label>}
      <div className="commit-assumption-actions">
        <button disabled={disabled} onClick={keep}>Keep</button>
        {editing ? <>
          <button disabled={disabled} onClick={() => setEditing(false)}>Cancel</button>
          <button disabled={disabled || !text.trim()} onClick={() => change(text)}>Ask Worker {label} to change</button>
        </> : <button disabled={disabled} onClick={() => setEditing(true)}>Change</button>}
      </div>
    </> : <p role="status">{assumption.state === 'kept' ? 'kept' : assumption.state === 'changing'
      ? `A change here goes back to ${label} first. Commit waits.` : 'changed for this commit'}</p>}
  </article>;
}

export function Assumptions({ job, disabled, keep, change }: {
  job: IntegrationJob; disabled: boolean;
  keep: (assumption: CommitAssumption) => void;
  change: (assumption: CommitAssumption, text: string) => void;
}) {
  if (!job.assumptions.length && !job.assumptionsChanged) return null;
  return <section className="commit-assumptions" aria-label="Commit assumptions">
    <h3>Assumptions</h3>
    {job.assumptionsChanged && <p role="status">Assumptions changed · Refresh to review them.</p>}
    {job.assumptions.map(assumption => assumption.included ? <Row
      key={`${assumption.id}:${assumption.revision}`} assumption={assumption}
      label={job.tracks.find(track => track.id === assumption.trackId)?.label ?? ''}
      disabled={disabled || job.assumptionsChanged} keep={() => keep(assumption)} change={text => change(assumption, text)} />
      : <p key={assumption.id}>Worker {job.tracks.find(track => track.id === assumption.trackId)?.label} · {assumption.text} · {assumption.state === 'changed' ? 'changed for this commit' : 'settled at the next commit'}</p>)}
  </section>;
}
