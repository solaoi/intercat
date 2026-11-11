import {useRef,useState} from 'react';
import type {ProjectSelection,ProjectSetupResult} from '../bridge';
import {usePrivacyLabels} from './PrivacyText';

export default function ProjectSelectionCard({selection,changed}:{selection:ProjectSelection;changed:(result:ProjectSetupResult)=>void}) {
  const [busy,setBusy]=useState(false),[error,setError]=useState('');
  const pending=useRef<'useRoot'|'retry'|'cancel'|null>(null);
  const issue=selection.issue;
  const roots=issue&&(issue.kind==='duplicate'||issue.kind==='repositoryRoot')?issue.roots:[];
  const text=issue?.kind==='duplicate'?'Same repository already added':issue?.kind==='repositoryRoot'?'This folder belongs to a repository':issue?.kind==='ignored'?'Ignored by its repository':'Repositories with submodules aren’t supported yet';
  const label=usePrivacyLabels([text,...roots,issue&&'path'in issue?issue.path:'',selection.error??'',error]);
  async function choose(action:'useRoot'|'retry'|'cancel') {
    setBusy(true);setError('');pending.current??=action;
    try {const result=await window.intercat.resolveProjectSelection({id:selection.id,action:pending.current});pending.current=null;changed(result);}
    catch(failure){setError(failure instanceof Error?failure.message:'The folder choice could not be saved.');}
    finally{setBusy(false);}
  }
  return <div className="desk-card desk-action-card project-selection">
    <span className="card-mark">◇</span>
    <div className="desk-copy">
      <strong>{issue?label(text):'Project choice needs attention'}</strong>
      {issue?.kind==='ignored'&&<p>Changes here can’t be committed.</p>}
      {issue&&'path'in issue&&<p className="project-selection-path">{label(issue.path)}</p>}
      {roots.length>0&&<details className="project-selection-roots"><summary>Repository roots ({roots.length})</summary><ul tabIndex={0} aria-label="Repository roots">{roots.map(root=><li key={root} className="project-selection-path">{label(root)}</li>)}</ul></details>}
      {(selection.error||error)&&<p role="alert">{label(error||selection.error!)}</p>}
    </div>
    <div className="desk-actions" data-card-actions>
      {pending.current?<button disabled={busy} onClick={()=>{void choose(pending.current!);}}>Retry saving folder choice</button>:<>
        {roots.length>0&&<button disabled={busy} onClick={()=>{void choose('useRoot');}}>Use the root</button>}
        {!issue&&<button disabled={busy} onClick={()=>{void choose('retry');}}>Retry saving project choice</button>}
        <button disabled={busy||selection.mayHaveSaved} onClick={()=>{void choose('cancel');}}>Cancel</button>
      </>}
    </div>
  </div>;
}
