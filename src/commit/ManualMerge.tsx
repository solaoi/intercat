import { useEffect, useRef, useState } from 'react';
import { monaco } from '../preview/editor';
import type { RepairRun } from './repair';
import { hasMarkers, hunks, type Apply, type Edit, type Hunk, type View } from './manual';

function ResultEditor({fileKey,value,readOnly,change,focusLine}:{fileKey:string;value:string;readOnly:boolean;change:(text:string)=>void;focusLine:number|null}) {
  const host=useRef<HTMLDivElement>(null);
  const pane=useRef<monaco.editor.IStandaloneCodeEditor|null>(null);
  const changed=useRef(change);changed.current=change;
  const updating=useRef(false);
  useEffect(()=>{
    if(!host.current)return;
    const model=monaco.editor.createModel(value,'plaintext');
    const editor=monaco.editor.create(host.current,{model,theme:'intercat',automaticLayout:true,fontSize:12,minimap:{enabled:false},scrollBeyondLastLine:false,readOnly,accessibilitySupport:'on',ariaLabel:'Merge result'});
    pane.current=editor;
    const listener=model.onDidChangeContent(()=>{if(!updating.current)changed.current(model.getValue(monaco.editor.EndOfLinePreference.TextDefined));});
    return()=>{listener.dispose();pane.current=null;editor.dispose();model.dispose();};
    // A file gets its own model; changing text retains the current model's undo history.
  },[fileKey]);
  useEffect(()=>{
    const editor=pane.current,model=editor?.getModel();
    if(editor&&model&&model.getValue(monaco.editor.EndOfLinePreference.TextDefined)!==value){updating.current=true;try{editor.executeEdits('conflict-choice',[{range:model.getFullModelRange(),text:value}]);}finally{updating.current=false;}}
    editor?.updateOptions({readOnly});
  },[value,readOnly]);
  useEffect(()=>{if(focusLine){pane.current?.setPosition({lineNumber:focusLine,column:1});pane.current?.revealLineInCenter(focusLine);pane.current?.focus();}},[focusLine]);
  return <div className="merge-result" ref={host} />;
}
export default function ManualMerge({run,label,targetLabel,changed,dirtyChanged}:{run:RepairRun;label:string;targetLabel:string;changed:()=>Promise<void>;dirtyChanged:(dirty:boolean)=>void}) {
  const realign=run.owner.kind==='realign',shared=run.owner.kind==='shared',restore=run.owner.kind==='restore';
  const [view,setView]=useState<View|null>(null);
  const [drafts,setDrafts]=useState<Record<string,Edit>>({});
  const [busy,setBusy]=useState(false);
  const [error,setError]=useState('');
  const [focusLine,setFocusLine]=useState<number|null>(null);
  const pending=useRef<Apply|null>(null);
  const selected=useRef<string|null>(null);
  const sequence=useRef(0);
  const [guard,setGuard]=useState(false);
  useEffect(()=>{dirtyChanged(Object.keys(drafts).length>0);},[drafts,dirtyChanged]);
  useEffect(()=>()=>dirtyChanged(false),[dirtyChanged]);
  async function read(path:string|null,discard=false) {
    const ticket=++sequence.current;setBusy(true);setError('');
    try{
      const result=await window.intercat.readManualMerge({id:run.id,revision:run.revision,path});
      if(ticket!==sequence.current)return;
      selected.current=result.file.sides.path;setView(result);
      if(discard){pending.current=null;setDrafts(current=>{const next={...current};delete next[result.file.sides.path];return next;});}
    }catch(failure){if(ticket===sequence.current)setError(failure instanceof Error?failure.message:'Could not read this conflict.');}
    finally{if(ticket===sequence.current)setBusy(false);}
  }
  useEffect(()=>{void read(selected.current);return()=>{sequence.current+=1;};},[run.id,run.revision]);
  useEffect(()=>{
    const focus=()=>{if(!pending.current)void read(selected.current);};
    window.addEventListener('focus',focus);return()=>window.removeEventListener('focus',focus);
  },[run.id,run.revision]);
  const file=view?.file;
  const path=file?.sides.path??'';
  const draft=drafts[path];
  const text=draft?.kind==='text'?draft.text:draft?.kind==='side'?(draft.side==='target'?file?.sides.target?.text:file?.sides.incoming?.text)??'':file?.text??'';
  const canEdit=file?.editable??false;
  const stale=!!draft&&draft.version!==file?.version;
  const frozen=busy||!!pending.current||!!run.externalEditor;
  let parts:Hunk[]=[],hunkError='';
  if(canEdit&&draft?.kind!=='side'){try{parts=hunks(text,run.id);}catch(failure){hunkError=failure instanceof Error?failure.message:'Edit the result directly.';}}
  function choose(edit:Edit){setError('');setDrafts(current=>({...current,[edit.path]:edit}));}
  function change(text:string){if(file&&!pending.current)choose({kind:'text',path,version:draft?.version??file.version,text});}
  function take(hunk:Hunk,side:'ours'|'theirs') {
    const lines=text.match(/[^\n]*\n|[^\n]+$/g)??[];
    change(lines.slice(0,hunk.startLine-1).join('')+hunk[side]+lines.slice(hunk.endLine).join(''));
  }
  async function apply() {
    if(!view)return;setBusy(true);setError('');
    try{
      const request=pending.current??{opId:await window.intercat.createOperationId(),id:run.id,revision:run.revision,files:view.paths.map(path=>drafts[path])};
      pending.current=request;
      await window.intercat.applyManualMerge(request);await changed();
    }catch(failure){setError(failure instanceof Error?failure.message:'The resolution could not be confirmed.');}
    finally{setBusy(false);}
  }
  async function external(){if(!file)return;setBusy(true);setError('');try{await window.intercat.openManualMerge({opId:await window.intercat.createOperationId(),id:run.id,revision:run.revision,path});await changed();}catch(failure){setError(failure instanceof Error?failure.message:'The editor could not be opened.');await changed();}finally{setBusy(false);}}
  async function returned(){setBusy(true);setError('');try{await window.intercat.returnManualMerge({opId:await window.intercat.createOperationId(),id:run.id,revision:run.revision});setGuard(false);await changed();}catch(failure){setError(failure instanceof Error?failure.message:'The editor return could not be confirmed.');}finally{setBusy(false);}}
  async function discardDrafts(){setBusy(true);setError('');try{await window.intercat.stopRepair({opId:await window.intercat.createOperationId(),id:run.id});await changed();}catch(failure){setError(failure instanceof Error?failure.message:'Could not return to the conflict choices.');}finally{setBusy(false);}}
  const complete=!!view&&view.paths.every(path=>{
    const choice=drafts[path];return !!choice&&(choice.kind!=='text'||!hasMarkers(choice.text,run.id));
  });
  return <section className="merge-editor" aria-label={`Resolve ${label}’s conflicts`}>
    <header><strong>Resolve {label}’s conflicts</strong><span>{restore?'For Restore':shared?'For the shared checkpoint':realign?'For realignment':'For this commit'}</span></header>
    {error&&<p role="alert">{error}</p>}
    {run.externalEditor&&<div className="merge-editor-guard" role="status"><p>Files may still be open in this merge’s worktree. Confirm before Intercat changes it.</p><div>{guard?<><button disabled={busy} onClick={()=>{void returned();}}>Go ahead</button><button onClick={()=>setGuard(false)}>Wait</button></>:<button onClick={()=>setGuard(true)}>Continue in Intercat</button>}</div></div>}
    {!view?<p role="status">Reading conflict files…</p>:<>
      <nav aria-label="Conflict files">{view.paths.map(path=><button key={path} aria-pressed={path===file?.sides.path} disabled={busy||!!pending.current} onClick={()=>{void read(path);}}>{path}<small>{drafts[path]?'Choice made':'Needs a choice'}</small></button>)}</nav>
      {file&&<><div className="merge-file-heading"><strong>{path}</strong><div><button disabled={frozen} onClick={()=>choose({kind:'side',path,version:file.version,side:'target'})}>Take {targetLabel}</button><button disabled={frozen} onClick={()=>choose({kind:'side',path,version:file.version,side:'incoming'})}>Take {label}</button>{canEdit&&<button disabled={frozen} onClick={()=>change(text)}>Edit</button>}<button disabled={busy||!!pending.current||!canEdit||file.text===null} onClick={()=>{void external();}}>Open in your editor</button></div></div>
        {stale&&<p role="alert">This file changed outside Intercat. Your draft has been kept. Reload this file before choosing its resolution again.</p>}
        {!canEdit?<p>Binary, link, deletion or large-file conflict. Choose a complete version. {!file.sides.target&&`${targetLabel} deleted this file. `}{!file.sides.incoming&&`${label} deleted this file.`}</p>:<>
          {draft?.kind==='side'&&<p role="status">Chose {draft.side==='target'?targetLabel:label}’s complete file.</p>}
          {!!parts.length&&<ol className="merge-hunks" aria-label="Unresolved hunks">{parts.map(hunk=><li key={hunk.startLine}><span>Lines {hunk.startLine}–{hunk.endLine}</span><div><button disabled={frozen} onClick={()=>take(hunk,'ours')}>Take {targetLabel}</button><button disabled={frozen} onClick={()=>take(hunk,'theirs')}>Take {label}</button><button disabled={frozen} onClick={()=>setFocusLine(hunk.startLine)}>Edit</button></div></li>)}</ol>}
          {hunkError&&<p role="status">{hunkError}</p>}
          <ResultEditor fileKey={`${run.id}:${path}:${file.version}`} value={text} readOnly={frozen||stale||draft?.kind==='side'} change={change} focusLine={focusLine} />
          {!hasMarkers(text,run.id)&&!draft&&<button disabled={frozen} onClick={()=>choose({kind:'current',path,version:file.version})}>Use edited file</button>}
        </>}
        <div className="merge-file-actions"><button disabled={busy} onClick={()=>{void read(path,true);}}>Reload this file</button><span role="status">{stale?'File changed':!draft?'Choose a resolution':draft.kind==='text'&&hasMarkers(draft.text,run.id)?'Intercat markers remain':'Resolution chosen'}</span></div>
      </>}
      <footer><p>{restore?'Your saved Restore will continue with this resolution.':shared?'The shared checkpoint will be prepared before realignment continues.':realign?'The worker’s files will be checked for newer edits before realignment.':'Check will run before the resolved changes can be committed.'}</p><div><button disabled={busy||!!pending.current} onClick={()=>{void discardDrafts();}}>Discard editor drafts</button><button disabled={busy||!!run.externalEditor||!pending.current&&(!complete||stale)} onClick={()=>{void apply();}}>{pending.current?'Retry resolution':restore?'Use resolution and restore':shared?'Use shared resolution':realign?'Use resolution and realign':'Use resolution and check'}</button></div></footer>
    </>}
  </section>;
}
