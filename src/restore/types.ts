import type {CommandPresentation,WorkerState} from '../bridge';
import type {AccessInspection,RealignSource} from '../commit/realign';
export interface Retirement {id:string;job:string;publication:string;session:string;folder:string;track:string;label:string;kind:'aside'|'leaveOut';phase:'stopping'|'saving'|'ready'|'aside'|'released';error:string|null}
export interface RestoreCandidate {id:string;track:string;label:string;title:string;folder:string;session:string}
export type RestoreCommand=Extract<CommandPresentation,{command:'restore'}>;
export interface RestoreSelection {entry:Retirement|null;presentation:RestoreCommand|null}
export interface RestoreRequest {opId:string;retirementId:string}
export type RestorePhase='queued'|'waiting-for-worker'|'waiting-for-processes'|'waiting-for-access'|'needs-merge'|'applying'|'held'|'done'|'failed';
export interface RestoreJob {request:RestoreRequest;trackId:string;sessionId:string;folderId:string;revision:number;phase:RestorePhase;keepProcesses:boolean;error:string|null;inspection:AccessInspection|null;restored:{trackId:string;sessionId:string|null;previousBase:string;base:string;checkpoint:WorkerState['checkpoint'];suggestRealign:boolean}|null;mergeInput:{id:string;revision:number;track:string;label:string;stage:'baseline'|'undo'|'files';paths:string[]}|null}
export interface RestoreControl {opId:string;id:string;revision:number;action:{kind:'retry'|'hold'|'keepProcesses'}|{kind:'dismissRealign';head:string}}
export interface RestoreAccess {opId:string;request:RestoreRequest;fingerprint:string}
export interface AsideStop {record:{request:RestoreRequest;track:string;session:string|null;error:string|null};state:'prepared'|'dispatched'|'complete'}
export interface AsideInfo {retirementId:string;groups:number|null;error:string|null;stop:AsideStop|null}
export interface RestoreFollowup {requestId:string;revision:number;track:string;label:string;target:string;source:RealignSource}
