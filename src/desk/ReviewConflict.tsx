import { useRef, useState } from 'react';
import type { ReviewFix } from '../bridge';
import type { AttentionCard } from './NeedsYou';

function Conflict({ fix, history, disabled, open, label }: { label: string; fix: ReviewFix; history: ReviewFix[]; disabled: boolean; open: () => void }) {
  const [busy,setBusy]=useState(false), [error,setError]=useState('');
  const pending=useRef<{opId:string;fixId:string;revision:number;action:'fixOnceMore'|'keepIt'}|null>(null);
  async function answer(action:'fixOnceMore'|'keepIt') {
    if (busy || disabled) return;
    setBusy(true);setError('');
    try {
      if (!pending.current || pending.current.fixId!==fix.id || pending.current.revision!==fix.revision) pending.current={opId:await window.intercat.createOperationId(),fixId:fix.id,revision:fix.revision,action};
      else if(pending.current.action!==action) throw new Error('Retry the previous choice first.');
      await window.intercat.commandReviewFix(pending.current);pending.current=null;
    } catch(error) {setError(error instanceof Error?error.message:'The review choice could not be saved.');}
    finally {setBusy(false);}
  }
  return <div className="desk-card review-conflict" role="group" aria-label={`Review needs you · ${label}`}>
    <h3>Review needs you · {label}</h3>
    <p className="review-location">{fix.path}</p>
    <p>{fix.reason ?? 'Review needs your decision.'}</p>
    <p>{fix.text}</p>
    <button className="quiet" onClick={open}>Open reviewed step</button>
    <details><summary>Review and fix history</summary><ol>{history.map(value=><li key={value.id}>{value.path} · {value.state}{value.reason ? ` · ${value.reason}` : ''}{value.attempts?.length ? <ol>{value.attempts.map(attempt=><li key={attempt.inputId}>{attempt.state}{attempt.reason ? ` · ${attempt.reason}` : ''}</li>)}</ol> : null}</li>)}</ol></details>
    <div className="review-fix-actions" data-card-actions><button disabled={disabled||busy||!!pending.current&&pending.current.action!=='fixOnceMore'} onClick={()=>{void answer('fixOnceMore');}}>Fix once more</button><button disabled={disabled||busy||!!pending.current&&pending.current.action!=='keepIt'} onClick={()=>{void answer('keepIt');}}>Keep it</button></div>
    {error&&<p role="alert">{error}</p>}
  </div>;
}
export function reviewFixCards(fixes: ReviewFix[], tracks:{id:string;label:string}[], disabled:boolean, open:(id:string)=>void):AttentionCard[] {
  return fixes.filter(fix=>fix.state==='blocked').map(fix=>({
    id:`review-fix:${fix.id}`,kind:'stopped',version:fix.revision,
    title:`Review needs you · ${tracks.find(track=>track.id===fix.trackId)?.label??fix.trackId}`,
    destination:{trackId:fix.trackId,label:tracks.find(track=>track.id===fix.trackId)?.label??fix.trackId},
    content:<Conflict label={tracks.find(track=>track.id===fix.trackId)?.label??fix.trackId} fix={fix} history={fixes.filter(value=>value.trackId===fix.trackId)} disabled={disabled} open={()=>open(fix.clipId)}/>
  }));
}
