import { useRef, useState } from 'react';
import type { BackupReview, FingerprintRecoveryState } from '../bridge';

export function FingerprintRecovery({ value, rebuild, apply, cancel }: { value: FingerprintRecoveryState; rebuild: (request: { opId: string; projectId: string }) => Promise<void>; apply: (request: { opId: string; acknowledgeUnverified: boolean }) => Promise<void>; cancel: (request: { opId: string }) => Promise<void> }) {
  const operations = useRef(new Map<string, string>());
  const panel = useRef<HTMLElement>(null);
  const [busy, setBusy] = useState('');
  const [error, setError] = useState('');
  const [review, setReview] = useState<BackupReview | null>(null);
  const [acknowledged, setAcknowledged] = useState<Record<string, boolean>>({});
  const [keepUnverified,setKeepUnverified] = useState(false);
  const pendingApply = review?.apply ?? value.pendingApply;
  const missing = value.sessions.length + value.projects.length > 0;
  async function restore(projectId: string) {
    if (busy) return;
    setBusy(projectId); setError('');
    try {
      const opId = operations.current.get(projectId) ?? await window.intercat.createOperationId();
      operations.current.set(projectId, opId);
      await rebuild({ opId, projectId });
      operations.current.delete(projectId);
      panel.current?.focus();
    } catch (error) { setError(error instanceof Error ? error.message : 'Fingerprint reconstruction failed.'); }
    finally { setBusy(''); }
  }
  async function inspect() {
    if (busy) return;
    setBusy('review'); setError('');
    try { setReview(await window.intercat.reviewBackup({ action: 'inspect' })); }
    catch (error) { setError(error instanceof Error ? error.message : 'The saved recovery records could not be reviewed.'); }
    finally { setBusy(''); }
  }
  async function stop(host: BackupReview['hosts'][number]) {
    if (busy || !host.root) return;
    setBusy(host.id); setError('');
    try {
      const key = `${host.id}:${host.root.pid}:${host.root.started_seconds}:${host.root.started_micros}`;
      const opId = operations.current.get(key) ?? await window.intercat.createOperationId();
      operations.current.set(key, opId);
      setReview(await window.intercat.reviewBackup({ action: 'stop', opId, hostId: host.id, root: host.root }));
    } catch (error) { setError(error instanceof Error ? error.message : 'The recorded processes could not be confirmed stopped.'); }
    finally { setBusy(''); }
  }
  async function compare(trackId: string) {
    if (busy) return;
    const key = `files:${trackId}`;
    setBusy(key); setError('');
    try {
      const opId = operations.current.get(key) ?? await window.intercat.createOperationId();
      operations.current.set(key, opId);
      setReview(await window.intercat.reviewBackup({ action: 'reviewFiles', opId, trackId }));
      operations.current.delete(key);
    } catch (error) { setError(error instanceof Error ? error.message : 'Current files could not be compared.'); }
    finally { setBusy(''); }
  }
  async function choose(session: BackupReview['sessions'][number], choice: 'pick-up' | 'discard-history') {
    if (busy || !review) return;
    const hasWorkspace = review.workspaces.some(workspace => workspace.trackId === session.trackId && workspace.sessionId === session.sessionId);
    const reviewId = hasWorkspace && review.fileReview?.trackId === session.trackId ? review.fileReview.opId : null;
    const acknowledgeUnread = acknowledged[session.sessionId] === true;
    const key = `choice:${session.sessionId}:${choice}:${reviewId}:${acknowledgeUnread}`;
    setBusy(key); setError('');
    try {
      const opId = operations.current.get(key) ?? await window.intercat.createOperationId();
      operations.current.set(key, opId);
      setReview(await window.intercat.reviewBackup({ action: 'chooseSession', opId, sessionId: session.sessionId, choice, reviewId, acknowledgeUnread }));
      operations.current.delete(key);
    } catch (error) { setError(error instanceof Error ? error.message : 'The recovery choice could not be saved.'); }
    finally { setBusy(''); }
  }
  async function finishRecovery() {
    if (busy) return;
    setBusy('apply'); setError('');
    try {
      const key='apply';
      const opId=pendingApply?.opId ?? operations.current.get(key) ?? await window.intercat.createOperationId();
      operations.current.set(key,opId);
      await apply({opId,acknowledgeUnverified:pendingApply?.acknowledgeUnverified ?? keepUnverified});
      operations.current.delete(key);
    } catch(error) {
      setError(error instanceof Error ? error.message : 'Recovery could not be completed.');
      try { setReview(await window.intercat.reviewBackup({ action: 'inspect' })); } catch { /* Keep the original failure visible. */ }
    }
    finally {setBusy('');}
  }
  async function cancelRecovery() {
    if (busy || !pendingApply || pendingApply.phase !== 'capturing') return;
    setBusy('cancel');setError('');
    try { await cancel({opId:pendingApply.opId}); setReview(null); operations.current.delete('apply'); }
    catch(error) {setError(error instanceof Error ? error.message : 'The recovery preparation could not be cancelled.');}
    finally {setBusy('');}
  }
  return <section ref={panel} tabIndex={-1} className="fingerprint-recovery" aria-label="Fingerprint recovery review">
    <strong>{missing ? 'Values may appear unmasked' : value.historyReviewRequired ? 'History review required' : 'Fingerprint confirmation differs'}</strong>
    <p>{missing ? 'History remains paused. The saved confirmation identifies these differences:' : 'History remains paused until recovery review is complete.'}</p>
    <ul>{value.sessions.map(item => <li key={item.sessionId}>Track <code>{item.trackId}</code> · session <code>{item.sessionId}</code>: {item.reason === 'missing' ? 'fingerprints missing from this database' : item.reason === 'changed' ? 'fingerprints differ from the saved confirmation' : 'no matching saved confirmation'}</li>)}
      {value.projects.map(item => <li key={item.projectId}>Project <code>{item.projectId}</code>: saved secret fingerprints {item.reason === 'missing' ? 'are missing from this database' : item.reason === 'changed' ? 'differ from the saved confirmation' : 'have no matching saved confirmation'}
        {item.reason !== 'unexpected' && <button disabled={!!busy} onClick={() => { void restore(item.projectId); }}>{busy === item.projectId ? 'Rebuilding…' : 'Rebuild saved fingerprints'}</button>}
      </li>)}</ul>
    {!!value.rebuiltProjects?.length && <p>Saved fingerprints rebuilt for {value.rebuiltProjects.length} project(s). The saved values were kept in Keychain.</p>}
    {value.historyReviewRequired ? <p>File and session review is still required before history can reopen.</p> : value.sessions.length === 0 && value.projects.length === 0 && <p>The difference is in the registration record. No missing session fingerprint was identified.</p>}
    {pendingApply && <div><p>Recovery is prepared. Finish saving its confirmation to reopen the reviewed workspace.</p><button disabled={!!busy} onClick={() => { void finishRecovery(); }}>{busy==='apply' ? 'Finishing recovery…' : 'Finish prepared recovery'}</button>{pendingApply.phase === 'capturing' && <button disabled={!!busy} onClick={() => { void cancelRecovery(); }}>Cancel preparation and review again</button>}</div>}
    <button disabled={!!busy} onClick={() => { void inspect(); }}>{busy === 'review' ? 'Reviewing saved records…' : 'Review saved files and processes'}</button>
    {review && <section aria-label="Saved recovery records">
      <p>History remains paused. {review.pendingOperations} unfinished operation(s) are recorded.</p>
      {review.sessions.length > 0 && <section aria-label="Session recovery choices">
        <p>Choose how to recover each affected session. Saving a choice keeps history paused.</p>
        {review.sessions.map(session => {
          const hasWorkspace = review.workspaces.some(workspace => workspace.trackId === session.trackId && workspace.sessionId === session.sessionId);
          const files = review.fileReview?.trackId === session.trackId ? review.fileReview : null;
          const verified = !!files && !files.stale && files.complete && files.attachments.complete;
          const unread = !hasWorkspace || (!!files && (!files.complete || !files.attachments.complete));
          const stopped = review.missingProcessRecords.length === 0 && !review.hosts.some(host => host.issue || host.processes > 0 || host.groups > 0);
          return <fieldset key={session.sessionId} disabled={!!busy || !!pendingApply || !stopped} className="recovery-choice">
            <legend>Track {session.trackId} · session {session.sessionId}</legend>
            {session.choice && <p>Saved choice: {session.choice.choice === 'pick-up' ? 'Pick up from files' : 'Discard history'}.{session.choice.stale ? ' The recovery context changed; review and choose again.' : ' Files will be checked again before recovery is applied.'}</p>}
            {hasWorkspace && !files && <p>Compare this track's current files before choosing.</p>}
            {!hasWorkspace && <p>This session has no matching workspace in the restored log. Discarding its history will keep existing files.</p>}
            {unread && <label><input type="checkbox" checked={acknowledged[session.sessionId] === true} onChange={event => setAcknowledged(current => ({ ...current, [session.sessionId]: event.target.checked }))} /> I understand that these files and originals are unverified and will be kept.</label>}
            <div>
              <button disabled={!session.canPickUp || !verified} onClick={() => { void choose(session, 'pick-up'); }}>Save Pick up from files</button>
              <button disabled={(hasWorkspace && (!files || files.stale)) || (unread && !acknowledged[session.sessionId])} onClick={() => { void choose(session, 'discard-history'); }}>Save Discard history</button>
            </div>
          </fieldset>;
        })}
      </section>}
      {review.missingProcessRecords.map(id => <p key={id}>Process ownership records are missing for <code>{id}</code>. Its stop could not be confirmed.</p>)}
      <ul>{review.workspaces.map(workspace => <li key={workspace.trackId}>
        Track <code>{workspace.trackId}</code> · <code>{workspace.path}</code>
        <p>{workspace.savedReference.matches ? 'Saved reference matches the log.' : 'Saved reference differs or is unavailable.'}</p>
        <p>{workspace.filesReviewed ? 'Current file contents compared.' : 'Current file contents still require review.'} {workspace.attachmentsReviewed ? 'Recorded originals and workspace copies compared.' : 'Attachment verification is still pending.'}</p>
        <button disabled={!!busy || !!pendingApply || review.missingProcessRecords.length > 0 || review.hosts.some(host => host.issue || host.processes > 0 || host.groups > 0)} onClick={() => { void compare(workspace.trackId); }}>{busy === `files:${workspace.trackId}` ? 'Comparing files…' : 'Compare files and originals'}</button>
        {review.fileReview?.trackId === workspace.trackId && <div>
          <p>{review.fileReview.files} file(s) compared with the last saved commit. {review.fileReview.stale && 'Files or originals changed after this review. Compare again.'}</p>
          <ul>{review.fileReview.changes.map(change => <li key={change.path}><code>{change.path}</code> · {change.kind}</li>)}</ul>
          {review.fileReview.protected.length > 0 && <p>Protected files were left unread: {review.fileReview.protected.join(', ')}</p>}
          {review.fileReview.unsupported.length > 0 && <p>Files requiring separate review: {review.fileReview.unsupported.join(', ')}</p>}
          <p>Verified originals: {review.fileReview.attachments.verifiedOriginals} / {review.fileReview.attachments.originals}. Matching workspace copies: {review.fileReview.attachments.copies.filter(copy => copy.state === 'matched').length}.</p>
          {review.fileReview.attachments.missingOriginals.length > 0 && <p>Missing originals: {review.fileReview.attachments.missingOriginals.join(', ')}</p>}
          {review.fileReview.attachments.invalidOriginals.length > 0 && <p>Originals could not be verified: {review.fileReview.attachments.invalidOriginals.join(', ')}</p>}
          {review.fileReview.attachments.unrecordedOriginals.length > 0 && <p>Original files absent from the restored log: {review.fileReview.attachments.unrecordedOriginals.join(', ')}</p>}
          <ul>{review.fileReview.attachments.copies.filter(copy => copy.state !== 'matched').map((copy, index) => <li key={index}>Copy needs review: <code>{copy.path ?? copy.attachmentId}</code> · {copy.state === 'changed' ? 'differs from the original or is unsafe' : copy.state === 'missing' ? 'missing from this workspace' : copy.state === 'unrecorded' ? 'not referenced by this session' : copy.state === 'unowned' ? 'outside recorded attachment storage' : copy.state === 'original-unverified' ? 'its saved original could not be verified' : copy.state === 'unsafe-directory' ? 'attachment directory is missing or unsafe' : 'attachment control file changed'}</li>)}</ul>
        </div>}
      </li>)}</ul>
      {review.hosts.length === 0 ? <p>No saved host records were found.</p> : <ul>{review.hosts.map(host => <li key={host.id}>
        <strong>{host.id.startsWith('summary/') ? 'History summary' : host.id.startsWith('repair/') ? 'Commit repair' : 'Worker'}</strong>{host.root && <> · PID {host.root.pid}</>}
        {host.workspace && <p><code>{host.workspace}</code></p>}{!host.knownToLog && <p>This host is absent from the restored operation log.</p>}
        {host.issue ? <p>{host.issue}</p> : <p>{host.processes} owned process(es) · {host.groups} process group(s)</p>}
        {host.root && !host.issue && (host.processes > 0 || host.groups > 0) && <button disabled={!!busy} aria-label={`Stop recorded processes for PID ${host.root.pid}`} onClick={() => { void stop(host); }}>{busy === host.id ? 'Confirming stop…' : 'Stop recorded processes'}</button>}
      </li>)}</ul>}
      {!pendingApply && <div className="recovery-choice">
        <p>Applying recovery retires the selected histories and closes affected pending Commit reviews. Files and saved commits are kept. New file pickups inspect the current files before asking for further instructions.</p>
        <label><input type="checkbox" checked={keepUnverified} onChange={event=>setKeepUnverified(event.target.checked)} /> Keep any unverified files unchanged.</label>
        <button disabled={!!busy || value.projects.length>0 || review.sessions.some(session=>!session.choice || session.choice.stale) || review.workspaces.some(workspace=>!workspace.lastReviewId) || review.missingProcessRecords.length>0 || review.hosts.some(host=>host.issue || host.processes>0 || host.groups>0)} onClick={() => { void finishRecovery(); }}>{busy==='apply' ? 'Applying recovery…' : 'Apply reviewed recovery'}</button>
      </div>}
    </section>}
    {error && <p role="alert">{error}</p>}
  </section>;
}
