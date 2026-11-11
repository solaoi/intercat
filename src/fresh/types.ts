import type { BranchOrigin, InputTarget, WorkerState } from '../bridge';
import type { AccessInspection } from '../commit/realign';
export interface FreshRequest { opId:string;trackId:string;sessionId:string }
export type FreshPhase='waiting-for-turn'|'captured'|'releasing'|'inspecting'|'needs-access'|'capturing-files'|'publishing-files'|'snapshot-ready'|'summarizing'|'summary-ready'|'summary-held'|'publishing-owner'|'owner-held'|'transferred'|'files-held'|'cancelling'|'cancelled';
export interface FreshJob {request:FreshRequest;revision:number;phase:FreshPhase;successorTrack:string;successorSession:string;checkpoint:WorkerState['checkpoint'];inspection:AccessInspection|null;files:WorkerState['checkpoint'];summaryReady:boolean;error:string|null}
export interface FreshCommand {opId:string;id:string;revision:number;action:{kind:'cancel'|'retry'}|{kind:'allow-access';fingerprint:string}}
export interface ConversationContinuation {operation:string;source:string;successor:string;sourceSession:string;sessionId:string;boardSession:string;origin:BranchOrigin;ready:boolean;opened:boolean}
export function currentWriter(track:string, continuations:ConversationContinuation[]):string|null {
  const seen=new Set<string>();
  while(!seen.has(track)) {seen.add(track);const next=continuations.find(value=>value.source===track);if(!next)return track;track=next.successor;}
  return null;
}
export function writingTarget(target:InputTarget,continuations:ConversationContinuation[]):InputTarget|null {
  if(target.mode!=='lock')return target;
  const trackId=currentWriter(target.trackId,continuations);
  return trackId?{mode:'lock',trackId}:null;
}
export const freshPhaseText:Record<FreshPhase,string>={
  'waiting-for-turn':'Waiting for the current step and checkpoint',captured:'Preparing to close the original conversation',
  releasing:'Closing the original conversation',inspecting:'Checking the current files','needs-access':'File access needs your confirmation',
  'capturing-files':'Saving the current files','publishing-files':'Finishing the saved files','snapshot-ready':'Preparing the handoff summary',
  summarizing:'Making the handoff summary','summary-ready':'Preparing the new conversation','summary-held':'The summary needs attention',
  'publishing-owner':'Finishing the conversation transfer','owner-held':'The conversation transfer needs attention',
  transferred:'Continued in a new conversation','files-held':'The saved files need attention',cancelling:'Cancelling and confirming that processes have stopped',cancelled:'Cancelled · earlier requests stay held',
};
export const freshCanCancel=(job:FreshJob)=>!['publishing-owner','owner-held','transferred','cancelling','cancelled'].includes(job.phase);
export const freshCanRetry=(job:FreshJob)=>['files-held','summary-held','owner-held'].includes(job.phase);
export function canPrepareFresh(worker:WorkerState):boolean {
  return !!worker.sessionId&&['ready','held','working','approval','question','checkpoint','sleeping','stopped'].includes(worker.phase)
    && (!!worker.checkpoint||!!worker.activeTurn||worker.checkpointPending);
}
