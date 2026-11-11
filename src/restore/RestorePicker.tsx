import type {Retirement,RestoreCandidate,RestoreCommand} from './types';
import {PrivacyText} from '../desk/PrivacyText';
import '../commit/commit.css';
import './restore.css';

export function savedRestoreEntry(candidate:RestoreCandidate,entries:Retirement[],session:string|undefined) {
  return entries.find(entry=>entry.id===candidate.id&&entry.track===candidate.track&&entry.folder===candidate.folder
    &&entry.session===candidate.session&&entry.session===session&&entry.kind==='aside'&&entry.phase!=='released');
}

export function RestorePicker({command,entries,session,connected,choose,close}:{command:RestoreCommand;entries:Retirement[];session:string|undefined;connected:boolean;choose:(entry:Retirement)=>void;close:()=>void}) {
  return <aside className="commit-sheet restore-sheet" aria-label="Restore review" data-restore-operation={command.operationId} data-focus-region tabIndex={-1} onKeyDown={event=>{if(event.key==='Escape'&&!event.nativeEvent.isComposing){event.stopPropagation();close();}}}>
    <div className="commit-scroll"><header className="commit-heading"><div><h2>Restore</h2><p>Choose a saved path</p></div><button aria-label="Hide Restore" onClick={close}>×</button></header>
      <p className="commit-notice">Review a worker before restoring it. Held requests remain held.</p>
      {!command.restore.length&&<p className="commit-empty">No set-aside paths were available for this request.</p>}
      {command.restore.map(candidate=>{const entry=savedRestoreEntry(candidate,entries,session);const reason=!entry?'This saved path is no longer set aside.':entry.phase!=='aside'?'This worker is still being set aside.':null;return <section className="restore-choice" key={candidate.id}>
        <strong>Worker {candidate.label}</strong><p><PrivacyText text={candidate.title}/></p>{reason&&<p role="status">{reason}</p>}
        <div className="restore-actions"><button aria-label={`Review Restore Worker ${candidate.label}`} disabled={!connected||!entry||entry.phase!=='aside'} onClick={()=>{if(entry)choose(entry);}}>Review Restore</button></div>
      </section>;})}
    </div><footer className="commit-footer"><p>Same track · same saved session</p><div><button onClick={close}>Hide</button></div></footer>
  </aside>;
}
