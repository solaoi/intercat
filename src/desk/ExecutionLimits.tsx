import { useEffect, useRef, useState } from 'react';
import type { ExecutionLimitsChange, ExecutionLimitsSetting } from '../bridge';

export function ExecutionLimits() {
  const [setting, setSetting] = useState<ExecutionLimitsSetting | null>(null);
  const [value, setValue] = useState('');
  const [planner, setPlanner] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef<ExecutionLimitsChange | null>(null);
  const generation = useRef(0);
  const valid = [value, planner].every(text => Number.isInteger(Number(text)) && Number(text) >= 1 && Number(text) <= 65535);

  async function load() {
    const request = ++generation.current;
    setBusy(true); setError('');
    try {
      const next = await window.intercat.getExecutionLimits();
      if (generation.current !== request) return;
      pending.current = null; setSetting(next); setValue(String(next.parallelWorkers)); setPlanner(String(next.plannerReviewerTurns));
    } catch { if (generation.current === request) setError('Execution limits could not be loaded.'); }
    finally { if (generation.current === request) setBusy(false); }
  }
  useEffect(() => { void load(); return () => { generation.current += 1; }; }, []);

  async function apply() {
    if (!setting || busy || !valid) return;
    const generationAtStart = generation.current;
    setBusy(true); setError('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), revision: setting.revision, parallelWorkers: Number(value), plannerReviewerTurns: Number(planner) };
      const next = await window.intercat.changeExecutionLimits(pending.current);
      if (generation.current !== generationAtStart) return;
      pending.current = null; setSetting(next); setValue(String(next.parallelWorkers)); setPlanner(String(next.plannerReviewerTurns));
    } catch (failure) {
      if (generation.current === generationAtStart) setError(failure instanceof Error ? failure.message : 'Execution limits could not be saved.');
    } finally { if (generation.current === generationAtStart) setBusy(false); }
  }

  return <section className="execution-limits" aria-label="Parallel workers">
    <h3>Agents · Parallel workers</h3>
    <label htmlFor="parallel-workers">Maximum running turns</label>
    <input id="parallel-workers" type="number" min="1" max="65535" step="1" value={value} disabled={!setting || busy || !!pending.current} onChange={event => setValue(event.target.value)} />
    <p>Applies across all sessions. Running turns finish; new turns wait when the limit is full.</p>
    <label htmlFor="planner-reviewer-turns">Planner and Reviewer turns</label>
    <input id="planner-reviewer-turns" type="number" min="1" max="65535" step="1" value={planner} disabled={!setting || busy || !!pending.current} onChange={event => setPlanner(event.target.value)} />
    <p>Counted separately from worker turns.</p>
    {setting && <div className="execution-limits-actions"><button disabled={busy || !valid || !pending.current && Number(value) === setting.parallelWorkers && Number(planner) === setting.plannerReviewerTurns} onClick={() => { void apply(); }}>{busy ? 'Saving…' : pending.current ? 'Retry execution limits' : 'Apply execution limits'}</button></div>}
    {!setting && !error && <p role="status">Loading parallel workers…</p>}
    {error && <div role="alert"><p>{error}</p><div className="execution-limits-actions"><button disabled={busy} onClick={() => { void load(); }}>Reload execution limits</button></div></div>}
  </section>;
}
