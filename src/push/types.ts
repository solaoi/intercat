import type {Folder} from '../bridge';
export interface Destination {fingerprint:string;display:string;credentials:boolean}
export interface PushChoice {folderId:string;remote:string;targetRef:string|null;through:string|null}
export interface PushRequest {opId:string;choices:PushChoice[]}
export interface PushMember {folder:Folder;slug:string;target:{sourceRef:string;sha:string;remote:string;targetRef:string;destinations:Destination[]};lfs:{git:Destination;upload:Destination;api:Destination}[];ssh:{destination:Destination;host:string;port:number;userFingerprint:string}[]}
export interface PushReview {request:PushRequest;projectId:string;sessionId:string;sessionName:string;members:PushMember[]}
export interface PushPresentation {review:PushReview;commits:Record<string,string[]>;additionalCommits:Record<string,number>}
export type PushPhase='queued'|'preparing'|'scanning'|'hooks'|'lfs'|'sending'|'uncertain'|'done'|'failed'|'cancelled'|'needs-review';
export interface PushRecipient {folderId:string;destination:Destination;phase:PushPhase;attempt:number;scans:{findings:{id:string;path:string;line:number;endLine:number;rule:string}[];unscannedLfs:string[]}[];receipt:{sha:string;reference:string;destination:Destination;changed:boolean}|null;reason:string|null}
export interface PushJob {id:string;revision:number;review:PushReview;stopping:boolean;recipients:PushRecipient[]}
export type PushAction={action:'confirm';review:string}|{action:'retry'|'cancel';id:string;revision:number};
export interface PushCommand {opId:string;action:PushAction}
export interface PushSelection {intent?:'pull';operationId?:string;folders:string[];through?:Record<string,string>;jobId?:string}
export const pushActive=(job:PushJob)=>job.recipients.some(recipient=>['queued','preparing','scanning','hooks','lfs','sending'].includes(recipient.phase));
export const pushDone=(job:PushJob)=>job.recipients.every(recipient=>recipient.phase==='done');
export const phaseLabel:Record<PushPhase,string>={queued:'Queued',preparing:'Reading destination',scanning:'Checking secrets',hooks:'Checking pre-push hook',lfs:'Sending LFS files',sending:'Sending Git ref',uncertain:'Send result unknown',done:'Pushed',failed:'Stopped',cancelled:'Cancelled','needs-review':'Review destination again'};
