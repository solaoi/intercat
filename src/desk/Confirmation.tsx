import { usePrivacyText } from './PrivacyText';
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { ConfirmationChange, ConfirmationPolicy, ConfirmationStatus, LiveTrack, Utterance } from '../bridge';
import { requestTitle } from './RequestTitle';

function BackToAutomatic({ value, saved }: { value: ConfirmationStatus; saved: (value: ConfirmationStatus) => void }) {
  const pending = useRef<ConfirmationChange | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => { if (pending.current?.revision !== value.revision) pending.current = null; }, [value.revision]);
  async function back() {
    if (busy) return;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), revision: value.revision, action: { kind: 'back-to-automatic' } };
      saved(await window.intercat.changeConfirmation(pending.current));
      pending.current = null;
    } catch (error) { setError(error instanceof Error ? error.message : 'The confirmation mode could not be changed.'); }
    finally { setBusy(false); }
  }
  return <><button disabled={busy} onClick={() => { void back(); }}>{busy ? 'Saving…' : 'Back to automatic'}</button>{error && <span role="alert">{error}</span>}</>;
}

export function ConfirmationSetting({ value, saved }: { value: ConfirmationStatus; saved: (value: ConfirmationStatus) => void }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<ConfirmationChange | null>(null);
  useEffect(() => { pending.current = null; setError(''); }, [value.revision]);
  async function change(policy?: ConfirmationPolicy) {
    setBusy(true); setError('');
    try {
      if (policy) pending.current = { opId: await window.intercat.createOperationId(), revision: value.revision, action: { kind: 'policy', policy } };
      if (!pending.current) return;
      saved(await window.intercat.changeConfirmation(pending.current));
      pending.current = null;
    } catch (error) { setError(error instanceof Error ? error.message : 'The confirmation preference could not be saved.'); }
    finally { setBusy(false); }
  }
  return <section className="confirmation-setting" aria-label="Editor confirmation mode">
    <label htmlFor="confirmation-policy">Editor · Confirmation mode</label>
    <select id="confirmation-policy" value={value.policy} disabled={busy || value.active} onChange={event => { void change(event.target.value as ConfirmationPolicy); }}>
      <option value="automatic">Automatic</option><option value="always">Always</option><option value="off">Off</option>
    </select>
    <p>{value.active ? 'Each new interpretation waits for your OK.' : value.policy === 'off' ? 'Automatic entry into confirmation mode is off.' : 'Asks for confirmation after more than 3 of your last 10 requests need an explicit correction.'}</p>
    <small>{value.rates.explicit} explicit · {value.rates.inferred} inferred corrections in the last {value.rates.requests} requests</small>
    {value.active && <BackToAutomatic value={value} saved={saved} />}
    {error && <p role="alert">{error}<button disabled={busy} onClick={() => { void change(); }}>Retry setting</button></p>}
  </section>;
}

export function ConfirmationNotice({ value }: { value?: ConfirmationStatus }) {
  const [recent, setRecent] = useState(false);
  useEffect(() => {
    setRecent(!!value?.notice);
    const timer = setTimeout(() => setRecent(false), 5000);
    return () => clearTimeout(timer);
  }, [value?.revision]);
  if (!value || (!value.active && !recent)) return null;
  return <p className="confirmation-notice" role="status">{recent ? value.notice : 'Confirmation mode · each interpretation waits for your OK'}</p>;
}

export function ConfirmationChip({ word, tracks, mode, edit, cut, confirmed, modeChanged }: {
  word: Utterance; tracks: LiveTrack[]; mode?: ConfirmationStatus; edit: () => void; cut: () => void;
  confirmed: (word: Utterance) => void; modeChanged: (value: ConfirmationStatus) => void;
}) {
  const privacy = usePrivacyText(requestTitle(word));
  const title = privacy.view?.text ?? privacy.error ?? 'Checking privacy…';
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<{ opId: string; utteranceId: string; revision: number; decisionId: string } | null>(null);
  const acceptButton = useRef<HTMLButtonElement>(null);
  useLayoutEffect(() => {
    const button = acceptButton.current;
    const strip = button?.closest('.desk-scroll');
    const showAction = () => {
      const focused = document.activeElement;
      if (focused instanceof HTMLElement && button?.closest('.confirmation-chip')?.contains(focused)) {
        focused.scrollIntoView({ block: 'nearest', inline: 'nearest' });
      } else if (!document.querySelector('.attention-card:not([hidden]):not([inert])')) {
        button?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
      }
    };
    showAction();
    const observer = new ResizeObserver(showAction);
    if (strip) observer.observe(strip);
    if (button?.parentElement) observer.observe(button.parentElement);
    return () => observer.disconnect();
  }, [word.id, word.confirmation?.decisionId]);

  const decision = word.decision;
  const track = tracks.find(track => track.id === decision?.trackId);
  const destination = decision?.kind === 'fork' ? 'New track' : track ? `Worker ${track.label}` : decision?.command ?? 'This request';
  const timing = decision?.kind === 'trim' ? `stop ${track?.label ?? 'the current turn'} and send` : decision?.boundary === 'when-told' ? 'when you say' : decision?.boundary === 'after-turn' ? 'after this turn' : 'now';
  const disabled = busy || !!word.editHold;
  async function accept() {
    if (disabled || !word.confirmation) return;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), utteranceId: word.id, revision: word.revision, decisionId: word.confirmation.decisionId };
      confirmed(await window.intercat.confirmUtterance(pending.current));
      pending.current = null;
    } catch (error) {
      setError(error instanceof Error ? error.message : 'This interpretation could not be confirmed.');
      try { confirmed(await window.intercat.getUtterance(word.id)); } catch { /* Keep the current interpretation available for retry. */ }
    } finally { setBusy(false); }
  }
  return <div className="request-chip confirmation-chip" role="group" aria-label="Interpretation waiting for OK" tabIndex={0}
    onKeyDown={event => { if (event.key === 'Enter' && event.target === event.currentTarget) { event.preventDefault(); void accept(); } }}>
    <button className="request-select" disabled={busy} onClick={edit} title={title}>
      <span>◆</span><span className="request-text">{title}</span>
      <small className="request-result">{destination} · {timing}</small>
    </button>
    <small className="confirmation-wait">waiting for OK{word.state === 'held' ? ' · held' : ''}</small>
    <button ref={acceptButton} disabled={disabled} onClick={() => { void accept(); }}>{busy ? 'Saving…' : error ? 'Retry OK' : 'OK'}</button>
    <details className="confirmation-menu"><summary>Change ▾</summary><div>
      <button disabled={disabled} onClick={edit}>Read as…</button>
      <button disabled={disabled} onClick={cut}>Take back</button>
      {mode?.active && <BackToAutomatic value={mode} saved={modeChanged} />}
    </div></details>
    {mode?.active && mode.suggested && mode.suggestedFor === word.id && <small className="confirmation-suggestion">back to automatic?</small>}
    {error && <span role="alert">{error}</span>}
  </div>;
}
