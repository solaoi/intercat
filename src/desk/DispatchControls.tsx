import type { DaemonStatus } from '../bridge';

export function DispatchControls({status, change}:{status:DaemonStatus;change:(paused:boolean)=>void}) {
  const running=(status.workers??[]).filter(worker=>worker.activeTurn).length
    +(status.repairRuns??[]).filter(run=>['queued','preparing','running'].includes(run.phase)).length;
  const active=running>0 || ['building','staging','committing'].includes(status.integration?.status??'');
  if (!active && !status.dispatchPaused) return null;
  const held=(status.utterances??[]).filter(word=>word.state==='held').length;
  return <>
    <button aria-label={'Stop all turns, '+running+' running'} onClick={()=>change(true)}>■ Stop all turns</button>
    {status.dispatchPaused && <span role="status">Turns stopped{status.runningProcesses ? ' · '+status.runningProcesses+' processes still running':''}{held ? ' · '+held+' held':''} <button disabled={running>0 || !!status.workers?.some(worker=>worker.checkpointPending)} onClick={()=>change(false)}>Resume</button></span>}
    {!!status.processIssues?.length && <span role="alert">Some process records could not be verified.</span>}
  </>;
}
