import type {PushReview} from '../push/types';
export interface PullDraft {title:string;body:string}
export interface PullResult {number:number;url:string;headSha:string;state:string}
export type PullPhase='drafting'|'looking-up'|'review'|'ready'|'creating'|'uncertain'|'checking'|'created'|'linking'|'done'|'failed'|'discarded';
export interface PullChoice {folderId:string;destination:string;base:string}
export interface PullOpen {opId:string;pushReview:string;choices:PullChoice[]}
export interface PullMember {folderId:string;name:string;destination:string;target:{repository:{owner:string;name:string};base:string;head:string;sha:string};draft:PullDraft|null;existing:PullResult|null;result:PullResult|null;phase:PullPhase;error:string|null}
export interface PullJob {id:string;revision:number;request:PullOpen;push:PushReview;account:{id:number;login:string}|null;confirmed:string|null;held:boolean;members:PullMember[]}
export type PullAction={action:'edit';id:string;revision:number;drafts:{folderId:string;draft:PullDraft}[]}|{action:'confirm'|'retry'|'reviewAgain'|'stop'|'discard';id:string;revision:number};
export interface PullCommand {opId:string;action:PullAction}
export interface PullSelection {operationId?:string;review:PushReview;jobId?:string}
export const pullRunning=(job:PullJob)=>!job.held&&job.members.some(member=>['drafting','looking-up','ready','creating','checking','created','linking'].includes(member.phase));
export const pullDone=(job:PullJob)=>job.members.every(member=>member.phase==='done');
export const pullLabel:Record<PullPhase,string>={drafting:'Drafting locally','looking-up':'Reading account and matching PRs',review:'Ready to review',ready:'Confirmed',creating:'Creating PR',uncertain:'Creation result unknown',checking:'Checking existing result',created:'PR saved',linking:'Updating PR text and related links',done:'PR ready',failed:'Stopped',discarded:'Discarded'};
