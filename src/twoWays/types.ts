import type { BranchStart } from '../bridge';
export type Phase='waiting'|'capturing-initial'|'inspecting'|'awaiting-usage-confirmation'|'inspecting-confirmed-usage'|'forking'|'awaiting-receipt'|'queued'|'held'|'cancelling'|'cancelled'|'declined';
export interface TwoWaysJob {request:{opId:string;inputId:string;revision:number;selection:TwoWaysSelection};revision:number;phase:Phase;sourceTrack:string;sourceSession:string;branchId:string;childInput:string;point:BranchStart|null;variation:{modelId:string;providerId:string;primary:string;alternate:string}|null;usageObservedAt:number|null;error:string|null;resume:Phase|null}
export interface TwoWaysCommand {opId:string;id:string;revision:number;action:'cancel'|'retry'|'confirm-usage'}
export const preparing=(job:TwoWaysJob)=>!['queued','cancelled','declined'].includes(job.phase);
export const phaseText:Record<Phase,string>={waiting:'Waiting for this instruction’s turn','capturing-initial':'Saving the files before the first step',inspecting:'Checking plan usage and the alternate effort','awaiting-usage-confirmation':'Plan usage is unavailable · confirm to run both paths','inspecting-confirmed-usage':'Rechecking plan usage and the alternate effort',forking:'Preparing the second path','awaiting-receipt':'Finishing preparation for both paths',queued:'Both instructions are queued',held:'Preparing both paths needs attention',cancelling:'Cancelling and confirming that preparation has stopped',cancelled:'Two-way preparation cancelled',declined:'The original path takes this instruction alone'};

export interface TwoWaysSelection {trackId:string;sessionId:string;variation:{modelId:string;providerId:string;primary:string;alternate:string}}
export interface TwoWaysOptions {inputId:string;revision:number;selection:TwoWaysSelection}

export interface PairedInstruction {inputId:string;effort:string;phase:'pending'|'active'|'finished'}
