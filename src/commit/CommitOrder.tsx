import { useState, type ReactNode } from 'react';
import type { CommitOrderReview, IntegrationJob } from './types';

export function CommitOrder({ job, review, busy, error, retry, confirm, close, reload, folderNavigation }: {
  folderNavigation:ReactNode; job: IntegrationJob; review: CommitOrderReview; busy: boolean; error: string;
  retry: (() => void) | null; confirm: (order: string[]) => void; close: () => void; reload: () => void;
}) {
  const [order, setOrder] = useState(review.order);
  const [editing, setEditing] = useState(false);
  const names = new Map(job.tracks.map(track => [track.id, track.label]));
  function move(index: number, by: number) {
    setOrder(current => { const next = [...current]; [next[index], next[index + by]] = [next[index + by], next[index]]; return next; });
  }
  return <aside className="commit-sheet commit-order" aria-label="Commit order proposal" data-focus-region tabIndex={-1}
    onKeyDown={event => { if (event.key === 'Escape' && !event.nativeEvent.isComposing) { event.stopPropagation(); close(); } }}>
    <header className="commit-heading"><div><h2>Before commit</h2><p>{job.project.name}</p></div><button aria-label="Hide commit order proposal" onClick={close}>×</button></header>
    {folderNavigation}
    <div className="commit-order-body">
      <h3>These tracks touch the same files</h3>
      <p>Choose their order before reviewing the commit. Only saved checkpoints are included.</p>
      <details><summary>{review.overlaps.length} shared file{review.overlaps.length === 1 ? '' : 's'}</summary>
        <ul>{review.overlaps.map(overlap => <li key={overlap.path}><code>{overlap.path}</code><span>{overlap.tracks.map(id => names.get(id)).join(' · ')}</span></li>)}</ul>
      </details>
      <ol aria-label="Commit order">{order.map((id, index) => {
        const track = job.tracks.find(track => track.id === id)!;
        return <li key={id}><span><strong>Worker {track.label}</strong><small>{track.title}</small></span>{editing && <div>
          <button disabled={busy || !!retry || index === 0} aria-label={`Move Worker ${track.label} earlier`} onClick={() => move(index, -1)}>↑</button>
          <button disabled={busy || !!retry || index === order.length - 1} aria-label={`Move Worker ${track.label} later`} onClick={() => move(index, 1)}>↓</button>
        </div>}</li>;
      })}</ol>
      <p>This proposal can wait. Closing it keeps the saved review.</p>
      {error && <div className="commit-error" role="alert">{error}<div><button disabled={busy} onClick={reload}>Reload review</button></div></div>}
    </div>
    <footer className="commit-footer"><div>
      <button disabled={busy || !!retry} onClick={() => { setEditing(!editing); if (editing) setOrder(review.order); }}>{editing ? 'Back' : 'Change order'}</button>
      <button className="commit-primary" disabled={busy} onClick={() => retry ? retry() : confirm(order)}>{busy ? 'Saving order…' : retry ? 'Retry order confirmation' : editing ? 'Use this order' : 'Review in this order'}</button>
    </div></footer>
  </aside>;
}
