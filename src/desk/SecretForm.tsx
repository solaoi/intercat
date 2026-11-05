import { useRef, useState } from 'react';
import type { SecretEntry } from '../bridge';

function validName(name: string) {
  return /^[A-Z][A-Z0-9_]{0,127}$/.test(name)
    && !['PATH', 'HOME', 'USER', 'LOGNAME', 'SHELL', 'TMPDIR', 'ENV', 'BASH_ENV', 'SHELLOPTS', 'SSH_AUTH_SOCK', 'NODE_OPTIONS', 'PYTHONPATH', 'PYTHONHOME'].includes(name)
    && !['XDG_', 'INTERCAT_', 'MUSE_', 'GIT_', 'DYLD_', 'LD_'].some(prefix => name.startsWith(prefix));
}
export function SecretForm({ submit, cancel, stop, stopping }: { submit: (entry: SecretEntry) => Promise<void>; cancel: () => void; stop: () => void; stopping: boolean }) {
  const [name, setName] = useState('');
  const [scope, setScope] = useState<'once' | 'project'>('project');
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const ids = useRef<{ opId: string; fingerprintOpId: string } | null>(null);
  const valid = validName(name) && value.length > 0 && !value.includes('\0') && new TextEncoder().encode(value).length <= 8192;
  async function send() {
    if (!valid || busy || stopping) return;
    setBusy(true); setError('');
    try {
      if (!ids.current) ids.current = { opId: await window.intercat.createOperationId(), fingerprintOpId: await window.intercat.createOperationId() };
      await submit({ ...ids.current, scope, name, value });
      setValue(''); cancel();
    } catch (error) {
      const message = error instanceof Error ? error.message : '';
      setError(message.includes('Project secret delivery was not confirmed') ? 'Review saved values in Settings · Agents · Project secrets before retrying.' : message.includes('Saved for this project') ? 'Saved for this project. Worker delivery was not confirmed; review this worker before retrying.' : scope === 'project' ? 'The project secret could not be confirmed. Check saved values in Settings before retrying.' : 'The secret could not be confirmed. Check this worker before retrying.');
    }
    finally { setBusy(false); }
  }
  return <div className="secret-form">
    <div className="secret-body">
    <p>The worker will stop, save its files, and restart with this environment variable.</p>
    <label>Variable name<input aria-label="Secret variable name" value={name} disabled={busy || stopping} autoComplete="off" spellCheck={false} onChange={event => { setName(event.target.value); ids.current = null; }} /></label>
    {name && !validName(name) && <small role="alert">Use an uppercase name such as NPM_TOKEN. System variables are reserved.</small>}
    <label>Value<input type="password" aria-label="Secret value" value={value} disabled={busy || stopping} autoComplete="off" onChange={event => { setValue(event.target.value); ids.current = null; }} /></label>
    {error && <p role="alert">{error}</p>}
    </div>
    <div className="secret-footer">
    <fieldset className="secret-scope-choice" disabled={busy || stopping}><label><input type="radio" name="secret-scope" checked={scope === 'project'} onChange={() => { setScope('project'); ids.current = null; }} />Save for this project</label><label><input type="radio" name="secret-scope" checked={scope === 'once'} onChange={() => { setScope('once'); ids.current = null; }} />This time only</label></fieldset>
    <small className="secret-scope">{scope === 'project' ? 'Saved to Keychain · every worker of this project from now on · remove in Settings · not kept in Intercat’s log.' : 'This time only · this worker, until its host stops. Not kept in Intercat’s log.'}</small>
    <div className="question-actions"><button disabled={!valid || busy || stopping} onClick={() => { void send(); }}>{busy ? 'Preparing…' : scope === 'project' ? 'Save and use' : 'Use for this worker'}</button><button disabled={busy} onClick={() => { setValue(''); cancel(); }}>Cancel</button><button disabled={stopping} onClick={() => { setValue(''); stop(); }}>Stop</button></div>
    </div>
  </div>;
}
