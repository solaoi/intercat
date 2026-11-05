import { useEffect, useRef } from 'react';
import { monaco } from './editor';

export const originalCode = 'export function add(a: number, b: number) {\n  return a - b;\n}\n';
export const modifiedCode = 'export function add(a: number, b: number) {\n  return a + b;\n}\n';

export default function DocumentPreview() {
  const element = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const original = monaco.editor.createModel(originalCode, 'plaintext');
    const modified = monaco.editor.createModel(modifiedCode, 'plaintext');
    const editor = monaco.editor.createDiffEditor(element.current!, {
      theme: 'intercat', readOnly: true, originalEditable: false, automaticLayout: true,
      minimap: { enabled: false }, renderSideBySide: true, fontSize: 12,
      fontFamily: 'JetBrains Mono', scrollBeyondLastLine: false,
    });
    editor.setModel({ original, modified });
    return () => { editor.dispose(); original.dispose(); modified.dispose(); };
  }, []);
  return <div className="document-preview" ref={element} aria-label="Sample code diff" />;
}
