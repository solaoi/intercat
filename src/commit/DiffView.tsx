import { useEffect, useRef } from 'react';
import { monaco } from '../preview/editor';
import type { Diff } from './types';
import type { DiffSelection } from './context';

export default function DiffView({ value, select }: { value: Diff; select?: (range: DiffSelection) => void }) {
  const host = useRef<HTMLDivElement>(null);
  const selectRef = useRef(select);
  selectRef.current = select;
  useEffect(() => {
    if (!host.current || value.binary || value.tooLarge) return;
    const extension = value.path.split('.').pop() ?? '';
    const language = ({ ts: 'typescript', tsx: 'typescript', js: 'javascript', jsx: 'javascript', json: 'json', py: 'python', rs: 'rust', md: 'markdown', css: 'css', html: 'html' } as Record<string, string>)[extension] ?? 'plaintext';
    const before = monaco.editor.createModel(value.before, language);
    const after = monaco.editor.createModel(value.after, language);
    const editor = monaco.editor.createDiffEditor(host.current, { readOnly: true, renderSideBySide: false, automaticLayout: true, theme: 'intercat', fontSize: 12, minimap: { enabled: false }, scrollBeyondLastLine: false, renderOverviewRuler: false });
    editor.setModel({ original: before, modified: after });
    const listeners = ([['before', editor.getOriginalEditor(), before], ['after', editor.getModifiedEditor(), after]] as const).flatMap(([side, pane, model]) => {
      const publish = (range: import('monaco-editor').Selection) => {
        const startLine = range.startLineNumber;
        const endLine = Math.max(startLine, range.endLineNumber - (range.endColumn === 1 && !range.isEmpty() ? 1 : 0));
        const quote = Array.from({ length: endLine - startLine + 1 }, (_, index) => model.getLineContent(startLine + index)).join('\n');
        if (quote.trim()) selectRef.current?.({ side, startLine, endLine, quote });
      };
      return [pane.onDidChangeCursorSelection(event => {
        if (['mouse', 'keyboard'].includes(event.source)) publish(event.selection);
      }), pane.onMouseUp(event => {
        const range = pane.getSelection();
        if (event.target.position && range) publish(range);
      })];
    });
    return () => { listeners.forEach(listener => listener.dispose()); editor.dispose(); before.dispose(); after.dispose(); };
  }, [value]);
  if (value.binary || value.tooLarge) return <p className="commit-empty">{value.binary ? 'Binary file — text preview unavailable.' : 'This file exceeds the 1 MiB preview limit.'}</p>;
  return <div className="commit-diff" ref={host} role="region" aria-label={`Diff for ${value.path}`} />;
}
