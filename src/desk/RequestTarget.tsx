import { usePrivacyLabels } from './PrivacyText';
import type { InputTarget } from '../bridge';

interface Props {
  target: InputTarget;
  tracks: { id: string; label: string; title: string }[];
  disabled: boolean;
  onChange: (target: InputTarget) => void;
}

export function RequestTarget({ target, tracks, disabled, onChange }: Props) {
  const label = usePrivacyLabels(tracks.map(track => track.title));
  const selected = target.mode === 'lock' ? tracks.find(track => track.id === target.trackId) : null;
  const value = target.mode === 'lock' ? target.trackId : '';
  return <label className="request-target" title={selected ? `Only ${selected.label} · ${label(selected.title)}` : 'Let the Editor choose a track'}>
    <select aria-label="Request destination" value={value} disabled={disabled}
      onChange={event => onChange(event.target.value ? { mode: 'lock', trackId: event.target.value } : { mode: 'none' })}>
      <option value="">Editor decides</option>
      {value && !selected && <option value={value}>Unavailable track</option>}
      {tracks.map(track => <option key={track.id} value={track.id}>Only {track.label} · {label(track.title)}</option>)}
    </select>
    <svg viewBox="0 0 12 12" width="10" height="10" aria-hidden="true"><path d="m3 4.5 3 3 3-3" fill="none" stroke="currentColor" strokeWidth="1.3" /></svg>
  </label>;
}
