export type RealignPhase='queued'|'waiting-for-worker'|'preparing'|'needs-processes'|'waiting-processes'|'stopping-processes'|'needs-access'|'waiting-access'|'needs-merge'|'held'|'failed'|'done'|'discarded';
export type RealignSource={kind:'commit';jobId:string;revision:number}|{kind:'reparent';jobId:string;revision:number;parent:string};
export interface RealignRequest {opId:string;trackId:string;source:RealignSource}
export function realignMergeOwner(job:RealignJob):import('./repair').RepairOwner {
  return job.sharedInput?{kind:'shared',id:job.sharedInput.id,revision:job.sharedInput.revision}:{kind:'realign',id:job.request.opId,revision:job.revision};
}
export interface AccessInspection {processes:{identity:{pid:number;started_seconds:number;started_micros:number};app:string;files:number;workingDirectory:boolean;fingerprint:string}[];unreadable:number;fingerprint:string}
export interface RealignJob {request:RealignRequest;sessionId:string;revision:number;phase:RealignPhase;keepProcesses:boolean;waitForAccess:boolean;stopCommand:string|null;stopCheckpoint:string|null;inspection:AccessInspection|null;error:string|null;mergeInput:string|null;mergeBoundary:string|null;sharedInput:{id:string;revision:number;track:string;label:string;checkpoint:number}|null;resolutions:{base:string;target:string;incoming:string;tree:string}[]}
export type RealignAction={kind:'keepProcesses'|'stopProcesses'|'wait'|'retry'|'discard'}|{kind:'goAhead';fingerprint:string};
export interface RealignControl {opId:string;id:string;revision:number;action:RealignAction}
export interface PendingRealignment {operationId:string;request:RealignRequest;state:'prepared'|'dispatched'}
export function realignHoldsWork(jobs:RealignJob[],pending:PendingRealignment[],track:string):boolean {
  return pending.some(item=>item.request.trackId===track)||jobs.some(job=>job.request.trackId===track&&['queued','waiting-for-worker','preparing','stopping-processes','needs-merge'].includes(job.phase));
}
