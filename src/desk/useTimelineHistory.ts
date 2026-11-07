import { useEffect, useRef, useState } from 'react';
import type { DaemonStatus, EditHistory, HistoryAction, HistoryRequest, Utterance } from '../bridge';

interface Pending { request: HistoryRequest; draftInput?: string }
export function useTimelineHistory(scope: string, history: EditHistory | undefined, receive: (status: DaemonStatus) => void, openDraft: (word: Utterance) => void) {
  const pending = useRef(new Map<string, Pending>());
  const inFlight = useRef(new Set<string>());
  const activeScope = useRef(scope);
  activeScope.current = scope;
  const [states, setStates] = useState<Record<string, { busy: boolean; error: string }>>({});
  function state(busy: boolean, error = '') { setStates(current => ({ ...current, [scope]: { busy, error } })); }
  async function perform(action?: HistoryAction, draftInput?: string) {
    if (inFlight.current.has(scope)) return;
    inFlight.current.add(scope);
    state(true);
    try {
      let operation = pending.current.get(scope);
      if (!operation) {
        if (!action) return;
        operation = { request: { opId: await window.intercat.createOperationId(), action }, draftInput };
        pending.current.set(scope, operation);
      }
      const status = await window.intercat.editHistory(operation.request);
      pending.current.delete(scope);
      if (activeScope.current === scope) {
        receive(status);
        if (operation.draftInput) {
          const word = status.utterances?.find(word => word.id === operation.draftInput);
          if (word) openDraft(word);
        }
      }
      state(false);
    } catch (cause) {
      state(false, cause instanceof Error ? cause.message : 'The history change could not be confirmed. Retry uses the same operation.');
    } finally {
      inFlight.current.delete(scope);
      setStates(current => ({ ...current, [scope]: { busy: false, error: current[scope]?.error ?? '' } }));
    }
  }
  async function reload() {
    if (inFlight.current.has(scope)) return;
    inFlight.current.add(scope);
    state(true);
    try {
      const status = await window.intercat.getStatus();
      pending.current.delete(scope);
      if (activeScope.current === scope) receive(status);
      state(false);
    } catch { state(false, 'The history could not be refreshed.'); }
    finally { inFlight.current.delete(scope); }
  }
  function step(direction: 'undo' | 'redo') {
    const edit = history?.[direction];
    if (!edit) return;
    if (edit.blocked) { state(false, edit.blocked); return; }
    void perform({ kind: direction, entryId: edit.entryId });
  }
  const shortcut = useRef(step);
  shortcut.current = step;
  useEffect(() => {
    const editingText = () => {
      const element = document.activeElement;
      return element instanceof Element && (element.matches('input,textarea') || element.closest('[contenteditable="true"],.monaco-editor'));
    };
    const unsubscribe = window.intercat.onHistoryShortcut(direction => {
      if (editingText()) void window.intercat.nativeHistory(direction).catch(() => {});
      else shortcut.current(direction);
    });
    const keydown = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey || event.key.toLowerCase() !== 'z' || event.isComposing || editingText()) return;
      event.preventDefault();
      event.stopPropagation();
      shortcut.current(event.shiftKey ? 'redo' : 'undo');
    };
    window.addEventListener('keydown', keydown, true);
    return () => { unsubscribe(); window.removeEventListener('keydown', keydown, true); };
  }, []);
  return {
    busy: states[scope]?.busy ?? false,
    error: states[scope]?.error ?? '',
    pending: pending.current.has(scope),
    step,
    retry: () => { void perform(); },
    reload: () => { void reload(); },
    restore: (word: { id: string; revision: number }, draft: boolean) => {
      void perform({ kind: draft ? 'restoreDraft' : 'restore', utteranceId: word.id, revision: word.revision }, draft ? word.id : undefined);
    },
  };
}
