import { useState } from 'react';
import type { ReportAnswer, ReportQuestion, DaemonStatus, Utterance, SecretEntry } from '../bridge';
import { SecretForm } from './SecretForm';
import { MarkdownMessage } from '../markdown/MarkdownMessage';
import type { AttentionCard } from './NeedsYou';

type Actions = {
  reply: (question: ReportQuestion) => void;
  respond: (question: ReportQuestion, answer: ReportAnswer) => void;
  takeBack: (word: Utterance) => void;
  retry: (word: Utterance) => void;
  provideSecret: (question: ReportQuestion, entry: SecretEntry) => Promise<void>;
  stop: (track: string) => void;
};
function ReportCard({ question, status, reply, respond, takeBack, retry, provideSecret, stop }: Actions & { question: ReportQuestion; status: DaemonStatus }) {
  const [secret, setSecret] = useState(false);
  const worker = status.workers?.find(worker => worker.trackId === question.trackId);
  const label = status.tracks?.find(track => track.id === question.trackId)?.label ?? '';
  const word = status.utterances?.find(word => word.id === question.replyUtteranceId);
  const late = question.askedBeforeLastMessage;
  const stopping = !!worker && ['stopping', 'reconnecting', 'lost', 'needs-recovery', 'stopped'].includes(worker.phase);
  const preparing = status.secretInputs?.some(input => input.owner.kind === 'work' && input.trackId === question.trackId && ['prepared', 'stopping', 'settled', 'restarting'].includes(input.state)) ?? false;
  return <section className={`report-question${secret ? ' secret-question' : ''}`} role="group" aria-label={`Question from Worker ${label}`}>
    <strong>Worker {label} · {preparing ? 'Preparing secret…' : word ? word.state === 'held' ? 'Reply not sent' : 'Reply waiting · sends after this turn' : question.kind === 'review' ? 'Did this report ask you something?' : 'Waiting for your answer'}</strong>
    {late && <small>asked before your last message</small>}
    <MarkdownMessage text={question.text || 'The worker finished without a final report. Review its work before continuing.'} />
    {secret ? <SecretForm stopping={stopping} stop={() => stop(question.trackId)} cancel={() => setSecret(false)} submit={entry => provideSecret(question, entry)} /> : <div className="report-question-actions" data-card-actions>{word ? <>
      {word.state === 'held' && <button disabled={preparing || stopping} onClick={() => retry(word)}>Start reply</button>}
      <button disabled={preparing || stopping || ['starting', 'sent', 'landed'].includes(word.state)} onClick={() => takeBack(word)}>Take back reply</button>
    </> : <>
      <button disabled={preparing || stopping} onClick={() => reply(question)}>Reply</button>
      {worker && worker.sessionId === question.sessionId && <button disabled={preparing || stopping} onClick={() => setSecret(true)}>Provide a secret</button>}
      {question.kind === 'review' && <><button disabled={preparing || stopping} onClick={() => respond(question, { action: 'continue' })}>Continue</button><button disabled={preparing || stopping} onClick={() => respond(question, { action: 'hold' })}>{question.state === 'held' ? 'Held' : 'Hold'}</button></>}
    </>}</div>}
  </section>;
}
export function reportCards({ status, ...actions }: Actions & { status: DaemonStatus }): AttentionCard[] {
  return (status.reportQuestions ?? []).map(question => ({
    id: `report-${question.id}`,
    kind: 'stopped',
    destination: { trackId: question.trackId, label: status.tracks?.find(track => track.id === question.trackId)?.label ?? '' },
    version: question.revision,
    title: `Question from Worker ${status.tracks?.find(track => track.id === question.trackId)?.label ?? ''}`,
    content: <ReportCard key={`${question.id}:${question.revision}`} question={question} status={status} {...actions} />,
  }));
}

export function ReportProgress({ status }: { status: DaemonStatus }) {
  return <>{status.reports?.map(report => <div key={report.id} className="desk-card" role="status">Worker {status.tracks?.find(track => track.id === report.trackId)?.label} · Checking the report for questions…</div>)}</>;
}
