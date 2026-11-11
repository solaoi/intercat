import type { IntegrationJob } from './types';
import type { RepairRun } from './repair';
import type { AttentionCard } from '../desk/NeedsYou';
import { PrivacyText } from '../desk/PrivacyText';

export function integrationFailure(job:IntegrationJob|null|undefined,repairs:RepairRun[],reviewOpen:boolean,open:()=>void):AttentionCard[] {
  if(reviewOpen||job?.status!=='failed'||!job.candidate)return [];
  const merge=job.candidate.state==='conflict';
  if(!merge&&!['check-failed','check-mutated'].includes(job.candidate.state))return [];
  const owner=job.tracks.find(track=>track.id===job.candidate?.failedTrack);
  if(merge&&!owner)return [];
  const run=repairs.find(run=>run.owner.kind==='commit'&&run.owner.id===job.id&&!['done','discarded'].includes(run.phase));
  if(run&&['queued','preparing','running','stopping','captured','applying'].includes(run.phase))return [];
  const paths=job.plan?.rows.find(row=>row.track===owner?.id)?.conflicts.map(conflict=>conflict.path)??[];
  const heading=merge?(run?.phase==='manual'?'Continue resolving the merge':'Merge needs attention'):job.candidate.state==='check-mutated'?'Check changed tracked files':`Integration failed${job.candidate.check?.code==null?'':` · exit ${job.candidate.check.code}`}`;
  return [{id:`integration:${job.id}`,kind:'blocked',title:merge?`Worker ${owner!.label} · merge for this commit`:'Integration needs attention',version:`${job.revision}:${run?.revision??0}`,content:<div className="integration-failure"><strong>{heading}</strong><p>{merge?(paths.length?<PrivacyText text={paths.join(' · ')}/>: 'Open the Commit review to resolve this conflict.'):'Choose the worker to repair the merged result in the Commit review.'}</p><div><button onClick={open}>{merge?'Open merge':'Open Commit review'}</button></div></div>}];
}
