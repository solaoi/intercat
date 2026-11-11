import type {IntegrationJob} from './types';
import type {RepairRun} from './repair';
import type {RealignJob} from './realign';
export interface CommitGroup {
  id:string;revision:number;sessionId:string;projectId:string;
  status:'open'|'committing'|'partial'|'done'|'discarded';
  members:{folderId:string;name:string;jobId:string|null}[];
  lastCommit:string|null;error:string|null;tracks:string[]|null;
}
export interface CommitGroupRequest {
  opId:string;id:string;revision:number;
  targets:{jobId:string;revision:number;head:string;tree:string;settingsHash:string}[];
}
export function groupRequest(group:CommitGroup,jobs:IntegrationJob[],opId:string):CommitGroupRequest {
  return {opId,id:group.id,revision:group.revision,targets:group.members.flatMap(member=>{
    if(!member.jobId)return[];
    const job=jobs.find(job=>job.id===member.jobId);
    if(!job?.candidate)throw new Error('A folder has no checked candidate. Review every folder before committing.');
    return [{jobId:job.id,revision:job.revision,head:job.candidate.head,tree:job.candidate.tree,settingsHash:job.candidate.settingsHash}];
  })};
}

export function groupReadiness(group:CommitGroup,jobs:IntegrationJob[],repairs:RepairRun[],realignments:RealignJob[],dirty:Set<string>) {
  let ready=0,done=0,waiting=0;
  const members=group.members.map(member=>{
    if(!member.jobId)return{...member,state:'No saved work selected'};
    const job=jobs.find(job=>job.id===member.jobId);
    if(job?.status==='done'){done++;return{...member,state:'Committed'};}
    const reparenting=realignments.some(item=>item.request.source.kind==='reparent'&&item.request.source.jobId===job?.id&&!['done','discarded'].includes(item.phase));
    const held=reparenting||repairs.some(run=>['commit','undo'].includes(run.owner.kind)&&run.owner.id===job?.id&&(!['done','discarded'].includes(run.phase)||!!run.refresh&&!run.refresh.error));
    const drafts=job?.tracks.some(track=>dirty.has(track.id));
    const checked=job?.status==='checked'&&job.candidate?.state==='checked'&&job.tracks.some(track=>track.included&&track.checkpoint)&&!job.assumptionsPending&&!job.assumptionsFailed&&!job.assumptionsChanged&&!held&&!drafts;
    if(checked)ready++;else waiting++;
    return {...member,state:drafts?'Message draft':reparenting?'Dependency realignment':held?'Repair in progress':checked?job?.candidate?.check?.command?'Check passed':'Ready · no check configured':job?.status==='building'||job?.status==='staging'?'Checking…':job?.status==='committing'?'Committing…':'Needs attention'};
  });
  return {members,ready,done,waiting,canCommit:ready>0&&waiting===0&&['open','partial'].includes(group.status)};
}
