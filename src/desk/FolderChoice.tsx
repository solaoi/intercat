import { useRef, useState } from 'react';
import type { DaemonStatus, Folder } from '../bridge';
import type { AttentionCard } from './NeedsYou';
import { usePrivacyText } from './PrivacyText';

function FolderButton({ folder, busy, choose }: { folder: Folder; busy: boolean; choose: (id: string) => void }) {
  const { view, error, retry } = usePrivacyText(folder.name);
  return <>
    <button disabled={busy || !view} onClick={() => choose(folder.id)}>{view?.text ?? 'Checking folder name…'}</button>
    {error && <button disabled={busy} onClick={retry}>Retry folder name</button>}
  </>;
}

function FolderChoice({ trackId, label, folders }: { trackId: string; label: string; folders: Folder[] }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<{ opId: string; trackId: string; folderId: string } | null>(null);
  const inFlight = useRef(false);
  async function choose(folderId: string) {
    if (inFlight.current) return;
    inFlight.current = true;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), trackId, folderId };
      await window.intercat.assignFolder(pending.current);
      // Keep the controls disabled until the saved assignment removes this card.
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : 'The folder choice could not be confirmed.');
      inFlight.current = false;
      setBusy(false);
    }
  }
  return <div className="desk-card desk-action-card folder-choice">
    <span className="card-mark">◇</span>
    <div className="desk-copy"><strong>Choose a folder for Worker {label}</strong>{error && <p role="alert">{error}</p>}</div>
    <div className="desk-actions" data-card-actions>{pending.current
      ? <button disabled={busy} onClick={() => { void choose(pending.current!.folderId); }}>{busy ? 'Saving choice…' : 'Retry folder selection'}</button>
      : folders.map(folder => <FolderButton key={folder.id} folder={folder} busy={busy} choose={id => { void choose(id); }} />)}</div>
  </div>;
}

export function folderChoiceCards(status: DaemonStatus): AttentionCard[] {
  if (!status.project?.folders.length) return [];
  return (status.tracks ?? []).filter(track => !status.trackFolders?.[track.id] && !status.workspaces?.some(workspace => workspace.trackId === track.id)).map(track => ({
    id: `folder-${track.id}`, kind: 'blocked', title: `Choose a folder for Worker ${track.label}`,
    destination: { trackId: track.id, label: track.label },
    content: <FolderChoice trackId={track.id} label={track.label} folders={status.project!.folders} />,
  }));
}
