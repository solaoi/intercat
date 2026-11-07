import { useCallback, useEffect, useRef, useState } from 'react';
import type { CommandPresentation, Utterance } from '../bridge';

interface Props {
  words: Utterance[];
  connected: boolean;
  blocked: boolean;
  previewOpen: boolean;
  open: (word: Utterance) => void;
  visible: (presentation: CommandPresentation) => boolean;
  updated: (word: Utterance) => void;
}

/** Presentation receipts remain pending until a foreground window opens the view. */
export function useApplicationCommands(props: Props) {
  const latest = useRef(props);
  latest.current = props;
  const active = useRef<Utterance | null>(null);
  const acknowledged = useRef(new Set<string>());
  const running = useRef(false);
  const mounted = useRef(true);
  const [focused, setFocused] = useState(() => document.hasFocus());
  const [error, setError] = useState('');
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    mounted.current = true;
    const focus = () => setFocused(document.hasFocus());
    const surfaceChanged = () => setAttempt(value => value + 1);
    window.addEventListener('focus', focus);
    window.addEventListener('blur', focus);
    document.addEventListener('toggle', surfaceChanged, true);
    return () => {
      mounted.current = false;
      window.removeEventListener('focus', focus);
      window.removeEventListener('blur', focus);
      document.removeEventListener('toggle', surfaceChanged, true);
    };
  }, []);

  const present = useCallback(async () => {
    const word = active.current;
    const presentation = word?.presentation;
    if (!word || !presentation || running.current) return;
    running.current = true;
    setError('');
    try {
      const current = await window.intercat.getUtterance(word.id);
      if (!mounted.current) return;
      if (current.state !== 'placed' || current.revision !== word.revision || current.editHold || current.confirmation
        || current.presentation?.receiptId !== presentation.receiptId) {
        latest.current.updated(current);
        active.current = null;
        setAttempt(value => value + 1);
        return;
      }
      latest.current.open(current);
      await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
      if (!mounted.current) return;
      if (!latest.current.visible(presentation)) throw new Error('The action closed before its display could be confirmed.');
      const saved = await window.intercat.resolveUtterance({
        opId: presentation.receiptId, utteranceId: word.id, revision: word.revision,
        resolution: { action: 'presented' },
      });
      if (mounted.current) {
        acknowledged.current.add(presentation.receiptId);
        latest.current.updated(saved);
        active.current = null;
        setAttempt(value => value + 1);
      }
    } catch (failure) {
      if (mounted.current) setError(failure instanceof Error ? failure.message : 'The displayed action could not be recorded.');
    } finally {
      running.current = false;
    }
  }, []);

  useEffect(() => {
    if (!props.connected) return;
    if (active.current) {
      const current = props.words.find(word => word.id === active.current?.id);
      if (!running.current && (!current?.presentation || current.revision !== active.current.revision)) {
        active.current = null;
        setError('');
      }
    }
    if (active.current || !focused) return;
    const word = props.words.filter(word => word.state === 'placed' && word.presentation && !acknowledged.current.has(word.presentation.receiptId) && !word.editHold && !word.confirmation)
      .toSorted((left, right) => left.createdAt - right.createdAt || left.id.localeCompare(right.id))[0];
    const previewBlocks = props.previewOpen && word?.presentation?.command !== 'branch-from';
    if (!word || ((props.blocked || previewBlocks || document.querySelector('dialog[open]')) && !props.visible(word.presentation!))) return;
    active.current = word;
    void present();
  }, [props.words, props.blocked, props.previewOpen, props.connected, focused, attempt, present, error]);

  return { error, retry: () => { void present(); } };
}
