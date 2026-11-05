import { useEffect, useRef, useState } from 'react';
import type { LiveTrack, ProjectSecretState, ProjectSecretUsage, ProjectSecretSettings } from '../bridge';

export function ProjectSecrets({ project, tracks }: { project: ProjectSecretState; tracks: LiveTrack[] }) {
  const [settings, setSettings] = useState<ProjectSecretSettings | null>(null);
  const state = settings?.project ?? project;
  const [uses, setUses] = useState<ProjectSecretUsage[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const pending = useRef(false);
  useEffect(() => {
    let disposed = false;
    void window.intercat.getProjectSecretSettings().then(value => { if (!disposed) setSettings(value); }).catch(() => { if (!disposed) setError('Saved values could not be read from Keychain. Close and reopen Settings to retry.'); });
    return () => { disposed = true; };
  }, [project.revision, project.ready]);
  useEffect(() => {
    let disposed = false;
    async function refresh() {
      if (pending.current) return; pending.current = true;
      try { const current = await window.intercat.getProjectSecretUsage(); if (!disposed) setUses(current); }
      catch { if (!disposed) setError('Process use could not be checked. Try again before applying changes.'); }
      finally { pending.current = false; }
    }
    void refresh(); const timer = setInterval(() => { void refresh(); }, 1000);
    return () => { disposed = true; clearInterval(timer); };
  }, []);
  async function act(action: () => Promise<unknown>) {
    if (busy) return; setBusy(true); setError('');
    try {
      await action();
      setSettings(await window.intercat.getProjectSecretSettings());
      setUses(await window.intercat.getProjectSecretUsage());
    } catch { setError('This change could not be confirmed. Review the saved values before retrying.'); setSettings(await window.intercat.getProjectSecretSettings().catch(() => null)); }
    finally { setBusy(false); }
  }
  return <section className="project-secrets" aria-label="Project secrets">
    <h3>Agents · Project secrets</h3>
    <p>Values are kept in Keychain. Removing a value stops it reaching new hosts; older hosts update before their next turn.</p>
    {!settings && <p role="status">Reading saved values from Keychain…</p>}
    {settings?.issue && <div className="project-secret-recovery" role="alert"><p>{settings.issue}</p><p>Remove any unapplied value, or try applying the saved values again.</p><button disabled={busy} onClick={() => { void act(() => window.intercat.retryProjectSecrets()); }}>Apply saved values</button></div>}
    {settings && state.names.length === 0 && settings.savedNames.length === 0 && uses.length === 0 && <small>No project secrets saved.</small>}
    {settings && [...new Set([...state.names, ...settings.savedNames, ...uses.flatMap(use => use.names)])].sort().map(name => {
      const live = uses.filter(use => use.names.includes(name));
      const hosts = live.flatMap(use => use.processes).filter(process => process.host).length;
      const children = live.flatMap(use => use.processes.map(process => ({ use, process }))).filter(entry => !entry.process.host);
      const active = settings.savedNames.includes(name);
      const applied = state.names.includes(name);
      return <div className="project-secret-row" key={name}>
        <div><strong>{name}</strong><span>{active ? settings.issue || !applied ? 'Saved to Keychain · not applied' : 'Saved to Keychain' : applied ? 'Removed from Keychain · not applied' : 'Removed'}{hosts > 0 && ` · still in use by ${hosts} host${hosts === 1 ? '' : 's'}`}{children.length > 0 && ` · in use by ${children.length} process${children.length === 1 ? '' : 'es'}`}</span></div>
        <div>{(active || applied) && <button disabled={busy} onClick={() => { void act(async () => window.intercat.removeProjectSecret({ opId: await window.intercat.createOperationId(), name, revision: settings.savedRevision })); }}>{active ? 'Remove' : 'Confirm removal of'} {name}</button>}{hosts > 0 && <button disabled={busy || !state.ready} onClick={() => { void act(() => window.intercat.applyProjectSecrets(state.revision)); }}>Apply now</button>}</div>
        {children.filter(entry => !entry.process.kept).map(({ use, process }) => <div className="secret-process" key={`${use.nonce}:${process.identity.pid}`}>
          <small>{use.repairRunId ? 'Repair for Worker' : 'Worker'} {tracks.find(track => track.id === use.trackId)?.label ?? ''}’s process {process.identity.pid} is still running.</small>
          <button disabled={busy || !process.canStop} title={process.canStop ? undefined : 'The process cannot be attributed to this workspace.'} onClick={() => { void act(() => window.intercat.decideProjectSecretProcess({ nonce: use.nonce, identity: process.identity, keep: false })); }}>Stop it</button>
          <button disabled={busy} onClick={() => { void act(() => window.intercat.decideProjectSecretProcess({ nonce: use.nonce, identity: process.identity, keep: true })); }}>Keep</button>
        </div>)}
      </div>;
    })}
    {!state.ready && !settings?.issue && <p role="status">Waiting for the saved values to be confirmed.</p>}
    {error && <p role="alert">{error}</p>}
  </section>;
}
