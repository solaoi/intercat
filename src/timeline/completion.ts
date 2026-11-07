import type { Track } from './model';

export function completionLabel(track: Track) {
  if (track.status !== 'done') return null;
  switch (track.completion?.result?.kind) {
    case 'done': return { text: 'done', mark: '✓', tone: 'result-done' };
    case 'partial': return { text: 'done · partial', mark: '◐', tone: 'result-partial' };
    case 'failed': return { text: 'failed', mark: '✗', tone: 'result-failed' };
    default: return { text: 'done · result unclear', mark: '−', tone: 'result-unclear' };
  }
}
