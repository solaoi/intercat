import { useEffect, useRef, useState } from 'react';
import { editorWorkerCount, monaco } from './editor';
import { originalCode, modifiedCode } from './DocumentPreview';
import { renderFixture } from './pdf';

export default function Diagnostics() {
  const editorElement = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const [result, setResult] = useState<Record<string, unknown>>({ state: 'running' });
  const [blocked, setBlocked] = useState<Record<string, boolean>>({});
  useEffect(() => {
    let disposed = false;
    const violations: string[] = [];
    const onViolation = (event: SecurityPolicyViolationEvent) => { violations.push(`${event.violatedDirective}: ${event.blockedURI}`); };
    window.addEventListener('securitypolicyviolation', onViolation);
    const original = monaco.editor.createModel(originalCode, 'plaintext');
    const modified = monaco.editor.createModel(modifiedCode, 'plaintext');
    const editor = monaco.editor.createDiffEditor(editorElement.current!, { theme: 'intercat', readOnly: true, automaticLayout: true, minimap: { enabled: false } });
    editor.setModel({ original, modified });
    void (async () => {
      const pdf = await renderFixture(canvas.current!);
      await new Promise<void>((resolve, reject) => {
        const started = Date.now();
        const timer = setInterval(() => {
          if (editor.getLineChanges()?.length) { clearInterval(timer); resolve(); }
          else if (disposed || Date.now() - started > 10000) { clearInterval(timer); reject(new Error('Monaco worker diff timed out')); }
        }, 50);
      });
      if (!disposed) setResult({ state: 'passed', monaco: { diffChanges: editor.getLineChanges()?.length, workers: editorWorkerCount }, pdf, violations });
    })().catch(error => { if (!disposed) setResult({ state: 'failed', error: String(error), violations }); });
    return () => { disposed = true; window.removeEventListener('securitypolicyviolation', onViolation); editor.dispose(); original.dispose(); modified.dispose(); };
  }, []);
  async function checkBlockedOperations() {
    let evalBlocked = false; let fetchBlocked = false;
    try { new Function('return 1')(); } catch { evalBlocked = true; }
    try { await fetch('https://example.invalid/csp-probe'); } catch { fetchBlocked = true; }
    setBlocked({ evalBlocked, fetchBlocked });
  }
  return <main className="diagnostics"><h1>Renderer diagnostics</h1><p>Local fixtures · real workers · packaged assets</p><div className="diagnostics-panel" ref={editorElement} /><canvas ref={canvas} aria-label="Local PDF rendering" /><pre className="diagnostics-result" data-testid="diagnostics">{JSON.stringify(result, null, 2)}</pre><button onClick={() => { void checkBlockedOperations(); }}>Test blocked operations</button><pre data-testid="blocked-operations">{JSON.stringify(blocked)}</pre></main>;
}
