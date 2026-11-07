import { useState } from 'react';
import type { Prompt, PromptAnswer, PromptResponse, InputQuestion, SecretEntry } from '../bridge';
import { SecretForm } from './SecretForm';

function valid(question: InputQuestion, answer?: PromptAnswer) {
  if (!answer || (answer.note && Array.from(answer.note).length > 500)) return false;
  if (answer.freeText !== undefined) return !!answer.freeText.trim() && Array.from(answer.freeText).length <= 500;
  if (question.selection.mode === 'single') return question.options.some(option => option.label === answer.selectedLabel);
  const labels = answer.selectedLabels ?? [];
  return new Set(labels).size === labels.length && labels.every(label => question.options.some(option => option.label === label)) && labels.length >= (question.selection.minSelections ?? 1) && labels.length <= (question.selection.maxSelections ?? question.options.length);
}
export function QuestionCard({ stopping, pendingSecret = false, prompt, label, respond, provideSecret }: { stopping: boolean; pendingSecret?: boolean; prompt: Prompt; label: string; respond: (prompt: Prompt, response: PromptResponse) => Promise<void>; provideSecret?: (prompt: Prompt, entry: SecretEntry) => Promise<void> }) {
  const [secret, setSecret] = useState(false);
  const [answers, setAnswers] = useState<Record<string, PromptAnswer>>({});
  const [free, setFree] = useState<Record<string, boolean>>({});
  const [explain, setExplain] = useState(false);
  const [explanation, setExplanation] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const waiting = busy || stopping || pendingSecret || prompt.phase !== 'open';
  const complete = prompt.request.questions.every(question => valid(question, answers[question.id]));
  const partial = prompt.request.questions.filter(question => valid(question, answers[question.id])).map(question => answers[question.id]);
  async function send(response: PromptResponse) {
    setBusy(true); setError('');
    try { await respond(prompt, response); }
    catch { setError('The answer could not be confirmed. Check the worker before trying again.'); }
    finally { setBusy(false); }
  }
  function choose(question: InputQuestion, label: string) {
    setAnswers(current => {
      const old = current[question.id];
      const selectedLabels = old?.selectedLabels ?? [];
      return { ...current, [question.id]: { questionId: question.id, note: old?.note, ...(question.selection.mode === 'single' ? { selectedLabel: label } : { selectedLabels: selectedLabels.includes(label) ? selectedLabels.filter(value => value !== label) : [...selectedLabels, label] }) } };
    });
    setFree(current => ({ ...current, [question.id]: false }));
  }
  return <section className="question-card" role="group" aria-label={`Questions for ${label}`}>
    <strong>{label} · {waiting ? 'Waiting for confirmation' : 'Needs your answer'}</strong>
    {secret && provideSecret ? <SecretForm stopping={stopping} stop={() => { void send({ action: 'stop', answers: partial }); }} cancel={() => setSecret(false)} submit={entry => provideSecret(prompt, entry)} /> : <>
    <div className="question-body">
      {explain ? <label>Explain instead<textarea aria-label="Explanation" value={explanation} disabled={waiting} onChange={event => setExplanation(event.target.value)} /><small>{Array.from(explanation).length} / 500</small></label> : prompt.request.questions.map(question => <fieldset key={question.id} disabled={waiting}>
        <legend>{question.header}</legend><p>{question.question}</p>
        {question.options.map(option => <div key={option.label} className="question-option"><label><input type={question.selection.mode === 'single' ? 'radio' : 'checkbox'} name={`${prompt.request.userInputId}:${question.id}`} checked={!free[question.id] && (answers[question.id]?.selectedLabel === option.label || answers[question.id]?.selectedLabels?.includes(option.label) || false)} onChange={() => choose(question, option.label)} />{option.label}</label>{option.description && <small>{option.description}</small>}{option.preview && <details><summary>Preview</summary><pre>{option.preview.content}</pre></details>}</div>)}
        <label><input type="checkbox" checked={free[question.id] ?? false} onChange={event => { setFree(current => ({ ...current, [question.id]: event.target.checked })); setAnswers(current => ({ ...current, [question.id]: { questionId: question.id, freeText: '', note: current[question.id]?.note } })); }} />Write an answer</label>
        {free[question.id] && <><textarea aria-label={`Answer to ${question.header}`} value={answers[question.id]?.freeText ?? ''} onChange={event => setAnswers(current => ({ ...current, [question.id]: { questionId: question.id, freeText: event.target.value, note: current[question.id]?.note } }))} /><small>{Array.from(answers[question.id]?.freeText ?? '').length} / 500</small></>}
        <label className="question-note">Note (optional)<textarea aria-label={`Note for ${question.header}`} value={answers[question.id]?.note ?? ''} onChange={event => setAnswers(current => ({ ...current, [question.id]: { ...(current[question.id] ?? { questionId: question.id }), note: event.target.value } }))} /></label>
      </fieldset>)}
    </div>
    {error && <p role="alert">{error}</p>}
    <footer className="question-actions" data-card-actions><button disabled={waiting || (explain ? !explanation.trim() || Array.from(explanation).length > 500 : !complete)} onClick={() => { void send(explain ? { action: 'explain', content: explanation } : { action: 'answer', answers: prompt.request.questions.map(question => answers[question.id]) }); }}>{explain ? 'Send explanation' : 'Send answers'}</button><button disabled={waiting} onClick={() => setExplain(!explain)}>{explain ? 'Use the options' : 'Explain instead'}</button>{provideSecret && <button disabled={waiting} onClick={() => setSecret(true)}>Provide a secret</button>}<button disabled={stopping} onClick={() => { void send({ action: 'stop', answers: partial }); }}>Stop</button></footer>
    </>}
  </section>;
}
