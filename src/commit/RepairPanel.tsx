import { useState } from 'react';
import type { Approval, Prompt, PromptResponse, OutputPreview, SecretEntry, SecretInputState } from '../bridge';
import { ProgressOutput } from '../desk/ProgressOutput';
import { ApprovalCard } from '../desk/ApprovalCard';
import { QuestionCard } from '../desk/QuestionCard';
import { SecretForm } from '../desk/SecretForm';
import { MarkdownMessage } from '../markdown/MarkdownMessage';
import type { RepairRun } from './repair';

export function RepairPanel({ run, label, preview, secretInputs, changed }: { secretInputs: SecretInputState[]; preview?: OutputPreview | null; run: RepairRun; label: string; changed: () => Promise<void> }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [secret, setSecret] = useState(false);
  const entries = secretInputs.filter(entry => entry.owner.kind === 'repair' && entry.owner.runId === run.id);
  const preparing = entries.some(entry => ['prepared', 'stopping', 'settled', 'restarting'].includes(entry.state));
  const waiting = run.phase === 'held' && !!run.session && !!run.turn && run.output?.split('\n').some(line => line.trimStart().startsWith('Waiting for you:'));
  const stopping = run.phase === 'stopping';
  const refreshing = !!run.refresh && !run.refresh.error;
  const title = `${label} · for the commit`;
  async function action(operation: () => Promise<unknown>) {
    setBusy(true); setError('');
    try { await operation(); await changed(); }
    catch (failure) { setError(failure instanceof Error ? failure.message : 'The repair action could not be confirmed.'); }
    finally { setBusy(false); }
  }
  async function stop() {
    // Stop remains available while an approval or another action awaits its acknowledgement.
    setError('');
    try { await window.intercat.stopRepair({ opId: await window.intercat.createOperationId(), id: run.id }); await changed(); }
    catch (failure) { setError(failure instanceof Error ? failure.message : 'The repair Stop could not be confirmed.'); }
  }
  async function control(kind: 'resume' | 'discard') {
    await action(async () => window.intercat.controlRepair({ opId: await window.intercat.createOperationId(), id: run.id, revision: run.revision, action: kind }));
  }
  async function answer(card: Approval, allow: boolean) {
    await action(async () => window.intercat.answerRepair({ opId: await window.intercat.createOperationId(), id: run.id, approvalId: card.id, requirement: card.requirement, allow }));
  }
  async function respond(prompt: Prompt, response: PromptResponse) {
    await action(async () => window.intercat.respondToRepair({ opId: await window.intercat.createOperationId(), id: run.id, promptId: prompt.request.userInputId, revision: prompt.revision, response }));
  }
  async function provideSecret(entry: SecretEntry, prompt?: Prompt) {
    const sessionId = prompt?.request.sessionId ?? run.session;
    const turnId = prompt?.request.turnId ?? run.turn;
    if (!sessionId || !turnId) throw new Error('The repair question is no longer available.');
    setBusy(true);
    try {
      await window.intercat.provideWorkerSecret({ ...entry, owner: { kind: 'repair', runId: run.id }, trackId: run.track, sessionId, turnId, question: prompt ? { kind: 'msp', id: prompt.request.userInputId, revision: prompt.revision } : { kind: 'report', id: run.id, revision: run.revision } });
      setSecret(false);
      await changed();
    } finally { setBusy(false); }
  }
  const state = { queued: 'Waiting for a worker', preparing: 'Preparing repair', running: 'Repairing', stopping: 'Stopping', held: 'Repair held', captured: 'Preparing Check', applying: 'Checking the repair', done: 'Repair finished', failed: 'Repair needs attention', 'needs-recovery': 'Repair needs recovery', discarded: 'Repair discarded' }[run.phase];
  return <section className="commit-repair" aria-label={title}>
    <header><strong>{title}</strong><span role="status">{run.restoring ? 'Restoring saved files' : refreshing ? 'Stopping repair and refreshing' : state}</span><div>
      {(['queued', 'preparing', 'running', 'stopping'].includes(run.phase) || preparing || busy) && <button disabled={stopping} onClick={() => { void stop(); }}>Stop repair</button>}
      {run.phase === 'held' && <button disabled={busy || preparing || refreshing || !!run.restoring} onClick={() => { void control('resume'); }}>Resume repair</button>}
      {['queued', 'held', 'captured', 'failed'].includes(run.phase) && <button disabled={busy || refreshing || !!run.restoring} onClick={() => { void control('discard'); }}>Discard repair</button>}
    </div></header>
    <div className="commit-repair-body">
      {run.prompts.map(prompt => <QuestionCard pendingSecret={preparing} provideSecret={(prompt,entry) => provideSecret(entry,prompt)} key={`${prompt.request.userInputId}:${prompt.revision}`} stopping={stopping} prompt={prompt} label={title} respond={respond} />)}
      {run.approvals.map(card => <ApprovalCard key={`${card.id}:${JSON.stringify(card.requirement)}`} stopping={stopping} detail={null} card={card} label={title} answer={answer} />)}
      <ProgressOutput label={title} preview={preview} session={run.session} />
      {run.output && <MarkdownMessage label={`${title} result`} text={run.output} />}
      {preparing && <p role="status">Saving the repair and preparing its secret…</p>}
      {waiting && !preparing && !refreshing && !run.restoring && (secret ? <SecretForm submit={entry => provideSecret(entry)} cancel={() => setSecret(false)} stop={() => { void stop(); }} stopping={stopping} /> : <button disabled={busy} onClick={() => setSecret(true)}>Provide secret</button>)}
      {entries.filter(entry => entry.detail).map(entry => <p key={entry.opId}>{entry.name}: {entry.detail}</p>)}
      {run.phase === 'needs-recovery' && <p>The previous repair did not close cleanly. Its saved edits need recovery before continuing.</p>}
      {run.phase === 'failed' && <p>The repair could not finish. Its saved edits have been kept.</p>}
      {run.refresh?.error && <p role="alert">{run.refresh.error}</p>}{error && <p role="alert">{error}</p>}
    </div>
  </section>;
}
