import { useEffect, useRef, useState } from 'react';
import type { EditorNotice, EditorPresentation, EditorReturn } from '../bridge';

export function useEditorPresentation(onReturn: (interval: EditorReturn) => void) {
  const [state, setState] = useState<EditorPresentation | null>(null);
  const [away, setAway] = useState<EditorNotice | null>(null);
  const [error, setError] = useState('');
  const returned = useRef(onReturn);
  returned.current = onReturn;
  const observed = useRef<string | null>(null);
  const completed = useRef<string | null>(null);
  const noticeId = useRef(-2);
  useEffect(() => {
    let disposed = false, changed = false;
    const stop = window.intercat.onEditorPresentation(value => { changed = true; if (!disposed) { setState(value); setError(''); } });
    void window.intercat.getEditorPresentation().then(value => { if (!disposed && !changed) { setState(value); setError(''); } })
      .catch(() => { if (!disposed) setError('Editor preferences could not be loaded.'); });
    return () => { disposed = true; stop(); };
  }, []);
  useEffect(() => {
    const interval = state?.returned;
    if (!interval) return;
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const acknowledge = async () => {
      try { await window.intercat.acknowledgeEditorReturn(interval.id); }
      catch { if (!disposed) timer = setTimeout(() => { void acknowledge(); }, 5000); }
    };
    if (completed.current === interval.id) {
      void acknowledge();
      return () => { disposed = true; clearTimeout(timer); };
    }
    if (state.hidden) return;
    if (observed.current !== interval.id) { observed.current = interval.id; returned.current(interval); }
    const load = async () => {
      try {
        const page = await window.intercat.getEditorAway({ from: interval.from, to: interval.to, before: null });
        if (disposed) return;
        const parts = [page.waiting.length ? `${page.waiting.join(', ')} ${page.waiting.length === 1 ? 'waits' : 'wait'} for you` : '',
          `${page.updates} ${page.updates === 1 ? 'update' : 'updates'}`, page.effects ? `${page.effects} ${page.effects === 1 ? 'effect' : 'effects'}` : ''].filter(Boolean);
        const at = Date.now();
        setAway({ id: noticeId.current--, trackId: null, inputId: null, scene: 'away', title: 'While you were away',
          line: `While you were away: ${parts.join(' · ')}`, detail: parts.join(' · '), createdAt: at, expiresAt: at + 12_000, away: { interval, page } });
        completed.current = interval.id;
        setError('');
        void acknowledge();
      } catch {
        if (!disposed) { setError('The away summary could not be loaded. Retrying…'); timer = setTimeout(() => { void load(); }, 5000); }
      }
    };
    void load();
    return () => { disposed = true; clearTimeout(timer); };
  }, [state?.returned?.id, state?.hidden]);
  return { state, away, error, setCharacter: async (value: 'speak' | 'mute') => {
    await window.intercat.setEditorCharacter(value);
    setState(await window.intercat.getEditorPresentation());
    setError('');
  } };
}
