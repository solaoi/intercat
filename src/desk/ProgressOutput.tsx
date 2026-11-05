import type { OutputPreview } from '../bridge';

export function ProgressOutput({ preview, session, label }: { preview?: OutputPreview | null; session: string | null; label: string }) {
  if (!preview || preview.sessionId !== session || !preview.text) return null;
  return <div className="worker-progress"><span>In progress</span><pre className="worker-output" role="log" aria-live="off" aria-label={`${label} in progress`}>{preview.text}{preview.truncated && '\n…'}</pre></div>;
}
